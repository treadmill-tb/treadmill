use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result, Context};
use async_recursion::async_recursion;
use async_trait::async_trait;
use clap::Parser;
use log::{debug, info, warn};
use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::image;
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use tml_tcp_control_socket_server::TcpControlSocket;

mod image_store_client;

async fn fuse<R>(duration: std::time::Duration, fire: impl std::future::Future<Output = R>) -> R {
    // Cancellable await:
    tokio::time::sleep(duration).await;

    // Boom:
    fire.await
}

#[derive(Parser, Debug, Clone)]
pub struct QemuSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SSHPreferredIPVersion {
    Unspecified,
    V4,
    V6,
}

impl Default for SSHPreferredIPVersion {
    fn default() -> Self {
        SSHPreferredIPVersion::Unspecified
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuConfig {
    /// Main QEMU binary to execute for a job.
    qemu_binary: PathBuf,

    /// `qemu-img` binary, to work with qcow2 files.
    qemu_img_binary: PathBuf,

    /// List of arguments to pass to the QEMU binary.
    ///
    /// These arguments support template strings using the
    /// [`strfmt`](https://docs.rs/strfmt/latest/strfmt/) crate.q
    ///
    /// The available template strings are:
    ///
    /// - `job_id`: UUID as a hyphenated string
    ///
    /// - `qcow2_disk`: main `qcow2` disk, which may be an overlay. In the case
    ///   that it is an overlay, it is set up such that all other layers can be
    ///   correctly resolved (relative to the current working directory)
    ///
    /// - `tcp_control_socket_listen_addr: full socket address, with IPv6
    ///   address properly enclosed in square brackets, e.g., `[::1]:8080`
    qemu_args: Vec<std::ffi::OsString>,

    /// Maximum "working" disk image to be allocated for a job, in bytes.
    ///
    /// These are thinly provisioned qcow2 CoW files, and so don't necessarily
    /// take up this much space. However, all images will be extended to this
    /// size, and the virtual machine can then resize its internal partitions
    /// accordingly.
    ///
    /// The runner will be unable to execute any images that have a disk image
    /// with a size larger than this limit (even though the sparse qcow2 file
    /// may be smaller), as otherwise the image exposed to the VM may cut off a
    /// part of the image at the end.
    working_disk_max_bytes: u64,

    tcp_control_socket_listen_addr: std::net::SocketAddr,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configurations for individual connector implementations. All are
    /// optional, and not all of them have to be supported:
    cli_connector: Option<tml_cli_connector::CliConnectorConfig>,

    qemu: QemuConfig,
}

#[derive(Debug)]
pub struct QemuSupervisorFetchingImageState {
    start_job_req: connector::StartJobRequest,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
pub struct QemuSupervisorImageFetchedState {
    start_job_req: connector::StartJobRequest,
    manifest: image::manifest::ImageManifest,
}

#[derive(Debug)]
pub struct QemuSupervisorJobRunningState {
    /// The qemu process handle:
    qemu_proc: tokio::process::Child,

    /// Control socket handle:
    control_socket: TcpControlSocket<QemuSupervisor>,
    // /// Set of rendezvous proxy connections:
    // ssh_rendezvous_proxies: Vec<rendezvous_proxy::RendezvousProxy>,
}

#[derive(Debug)]
pub enum QemuSupervisorJobState {
    /// State to indicate that the job is starting.
    ///
    /// We use this to reserve a spot in the [`QemuSupervisor`]'s `jobs` map,
    /// such that we can release the global HashMap lock afterwards.
    Starting,

    /// State to indicate that we're currently waiting on the image to
    /// be fetched. This state polls the image store with a fixed
    /// interval.
    FetchingImage(QemuSupervisorFetchingImageState),

    /// The job's image has been fully fetched, and we're alloating resources
    /// and starting it. It's not fully up and running yet.
    ImageFetched(QemuSupervisorImageFetchedState),

    /// State to indicate that the job is running.
    Running(QemuSupervisorJobRunningState),

    /// State to indicate that the job is currently shutting down.
    ///
    /// While the job is in this state, no job with the same ID must be started
    /// / resumed. We might still be cleaning up resources associated with this
    /// job.
    Stopping,
}

impl QemuSupervisorJobState {
    fn state_name(&self) -> &'static str {
        match self {
            QemuSupervisorJobState::Starting => "Starting",
            QemuSupervisorJobState::FetchingImage(_) => "FetchingImage",
            QemuSupervisorJobState::ImageFetched(_) => "ImageFetched",
            QemuSupervisorJobState::Running(_) => "Running",
            QemuSupervisorJobState::Stopping => "Stopping",
        }
    }
}

pub struct QemuSupervisor {
    /// Connector to the central coordinator. All communication is mediated
    /// through this connector.
    connector: Arc<dyn connector::SupervisorConnector>,

    /// Image store client, connected to the local image cache. We expect to be
    /// provided an image store client with a filesystem endpoint, from which we
    /// can directly reference (immutable) qcow2 images.
    image_store_client: image_store_client::LocalImageStoreClient,

    /// We support running multiple jobs on one supervisor (in particular when
    /// not sharing hardware resources), so use a map of `Arc`s behind a mutex
    /// to avoid locking the map across long-running calls.
    jobs: Mutex<HashMap<Uuid, Arc<Mutex<QemuSupervisorJobState>>>>,

    args: QemuSupervisorArgs,
    config: QemuSupervisorConfig,
}

impl QemuSupervisor {
    pub fn new(
        connector: Arc<dyn connector::SupervisorConnector>,
        image_store_client: image_store_client::LocalImageStoreClient,
        args: QemuSupervisorArgs,
        config: QemuSupervisorConfig,
    ) -> Self {
        QemuSupervisor {
            connector,
            image_store_client,
            jobs: Mutex::new(HashMap::new()),
            args,
            config,
        }
    }

    async fn remove_job(&self, job_id: Uuid) -> Option<Arc<Mutex<QemuSupervisorJobState>>> {
        self.jobs.lock().await.remove(&job_id)
    }

    #[async_recursion]
    async fn fetch_image(this: &Arc<Self>, job_id: Uuid) {
        // This function can either be called directly from `start_job`, or by
        // polling on the image fetch operation.
        //
        // It may be possible that we cancel the poll task, but race with it
        // already scheduling another invocation of this `fetch_image` function.
        // However, in this case, we'll also have transitioned the job into the
        // `ImageFetched` state. Thus, if the job is in any state other than
        // `FetchingImage`, we just exit without doing anything:
        let job_opt = {
            // Do not hold onto the global job's lock:
            this.jobs.lock().await.get(&job_id).cloned()
        };

        // If the job is no longer alive, return:
        let job = match job_opt {
            Some(job) => job,
            None => {
                // This case should be pretty rare but does not indicate a bug
                // per se, so let's issue a debug message just in case:
                debug!(
                    "Job {:?} vanished across invocations of `fetch_image`",
                    job_id
                );
                return;
            }
        };

        // Acquire a lock on the job:
        let mut job_lg = job.lock().await;

        // We swap in a state of `Starting` temporarily to please the Rust
        // borrow checker. Before we leave this function, we must replace this
        // state again!
        //
        // If the job is in any state other than `FetchingImage`, swap back and
        // return immediately:
        let fetching_image_state =
            match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Starting) {
                QemuSupervisorJobState::FetchingImage(state) => state,
                prev_state => {
                    // Swap the old state back:
                    *job_lg = prev_state;

                    // Same as above, let's issue a debug message:
                    debug!(
                        "Job {:?} changed state across invocations of \
			 `fetch_image`: {:?}",
                        job_id, job,
                    );

                    return;
                }
            };

        let image_id = image::manifest::ImageId(fetching_image_state.start_job_req.image_id);

        // Check whether the image store already holds this image. If it does,
        // we can pass the manifest to the `start_job_cont` function. Otherwise,
        // (continue) polling:
        match this
            .image_store_client
            .fetch_image(
                // TODO: pass remote store endpoints:
                vec![],
                image_id,
            )
            .await
        {
            Ok(image_store_client::FetchImageStatus::Present) => {
                // Image is fetched, retrieve its manifest and pass it onto
                // `start_job_cont`. Retrieving the manifest should not fail.
                //
                // Don't need to post a status update to the coordinator
                // here. If we didn't need to fetch an image, this will simply
                // skip reporting the `FetchingImage` starting stage, and if we
                // do need to fetch one this will be reported below.
                //
                // The transition to `start_job_cont` will then report a
                // subsequent status, such as `Allocating`.

                let manifest = match this.image_store_client.image_manifest(image_id).await {
                    Ok(manifest) => manifest,
                    Err(manifest_err) => {
                        // We could not retrieve the manifest, despite the image
                        // being present. Don't attempt to recover from this
                        // error and mark the job as failed:
                        this.connector
                            .report_job_error(
                                fetching_image_state.start_job_req.job_id,
                                connector::JobError {
                                    request_id: Some(fetching_image_state.start_job_req.request_id),
                                    error_kind: connector::JobErrorKind::InternalError,
                                    description: format!(
                                        "Image store claims that image {:?} is fetched, \
				     but cannot retrieve its manifest: {:?}",
                                        image_id, manifest_err,
                                    ),
                                },
                            )
                            .await;

                        // No resources have been allocated (yet), so simply
                        // remove the job and set its state to `Stopping`, in
                        // case anyone else has a reference to it still.
                        //
                        // Safe to call, we don't hold a lock on `this.jobs`:
                        this.remove_job(job_id).await;

                        // Prevent other tasks from further advancing this job's state:
                        *job_lg = QemuSupervisorJobState::Stopping;

                        return;
                    }
                };

                // Place the job in the `ImageFetched` state and continue
                // starting it. This prevents any other poll task firing this
                // function from calling `start_job_cont` twice:
                *job_lg = QemuSupervisorJobState::ImageFetched(QemuSupervisorImageFetchedState {
                    start_job_req: fetching_image_state.start_job_req,
                    manifest,
                });

                // Release the lock before continuing to start, otherwise this
                // will deadlock:
                std::mem::drop(job_lg);
                Self::start_job_cont(this, job_id).await
            }

            Ok(image_store_client::FetchImageStatus::InProgress(msg)) => {
                // We still need to wait a bit until the image is available.
                // Poll again in 15 sec, and post this status to the
                // coordinator.

                // Start a new task that will run this function in 15 sec. We'll
                // want to cancel it when we perform any state transition
                // outside of this function:
                let poll_task_this_weak = Arc::downgrade(this);
                let poll_task =
                    tokio::task::spawn(fuse(std::time::Duration::from_secs(15), async move {
                        if let Some(poll_task_this) = poll_task_this_weak.upgrade() {
                            Self::fetch_image(&poll_task_this, job_id).await
                        }
                    }));

                this.connector
                    .update_job_state(
                        fetching_image_state.start_job_req.job_id,
                        connector::JobState::Starting {
                            stage: connector::JobStartingStage::FetchingImage,
                            status_message: msg,
                        },
                    )
                    .await;

                // Put the job into the `FetchingImage` state:
                *job_lg = QemuSupervisorJobState::FetchingImage(QemuSupervisorFetchingImageState {
                    poll_task: Some(poll_task),
                    ..fetching_image_state
                });
            }

            Err(e) => {
                // We could not fetch the image, report an error:
                this.connector
                    .report_job_error(
                        fetching_image_state.start_job_req.job_id,
                        connector::JobError {
                            request_id: Some(fetching_image_state.start_job_req.request_id),
                            error_kind: connector::JobErrorKind::InternalError,
                            description: format!("Failed to fetch image {:?}: {:?}", image_id, e),
                        },
                    )
                    .await;

                // No resources have been allocated (yet), so simply remove the
                // job and set its state to `Stopping`, in case anyone else has
                // a reference to it still.
                //
                // Safe to call, we don't hold a lock on `this.jobs`:
                this.remove_job(job_id).await;

                // Prevent other tasks from further advancing this job's state:
                *job_lg = QemuSupervisorJobState::Stopping;
            }
        }
    }

    async fn start_job_parse_manifest_check_images(&self) {
	// Retrieve the "top-most" layer in the image manifest, from which we'll
        // create our "working" disk image overlay:
	let top_layer_label = manifest.attrs
	    .get("org.tockos.treadmill.image.qemu_layered_v0.head")
	    .ok_or(anyhow!("Image does not have a head layer defined"))?;

	// Now step through each image layer, making sure that it exists and (if
	// it has a predecessor defined) it points to that predecessor. We don't
	// want images to be able to reference arbitrary paths in our filesystem
	// as underlying layers.
	//
	// We use qemu-img to retrieve the (partial) metadata of an image.
	#[derive(Deserialize)]
	#[serde(rename_all = "kebab-case")]
	struct QemuImgChildMetadata {
	    // We're only interested in the filename here, to make sure that the
	    // image has only one child node, and that node coincides with the
	    // file that we're operating on.
	    filename: PathBuf,
	}

	#[derive(Deserialize)]
	#[serde(rename_all = "kebab-case")]
	struct QemuImgMetadata {
	    filename: PathBuf,
	    virtual_size: u64,
	    children: Vec<QemuImgChildMetadata>,
	    encrypted: Option<bool>,
	    backing_filename_format: Option<String>,
	    // According to [1], these attributes are described as
	    // - backing-filename: name of the backing file
	    // - full-backing-filename: full path of the backing file
	    //
	    // In practice it seems that `full-backing-filename` points to the
	    // resolved path of the backing file (relative to the current
	    // working directory), whereas `backing-filename` is just the raw
	    // attribute stored in the image.
	    //
	    // [1]: https://www.qemu.org/docs/master/interop/qemu-storage-daemon-qmp-ref.html
	    backing_filename: Option<PathBuf>,
	    full_backing_filename: Option<PathBuf>,
	}

	// Now, for every image in the chain, parse the metadata and make sure
	// that the image file:
	//
	// - Exists,
	// - Matches its specification in the manifest, and
	// - Only references its defined predecessor.
	let mut current_image = top_layer_label;
	loop {
	    // Acquire the manifest metadata for this image "part":
	    let part_spec = manifest.parts.get(current_image)
		.ok_or_else(|| anyhow!("Image missing part {}", current_image))?;

	    // Try to read image attributes with `qemu-img`:
	    let image_path = this.image_store_client.part_path(&part_spec.sha256_digest).await;

	    let image_path_canon = tokio::fs::canonicalize(&image_path)
		.await
		.with_context(|| format!(
		    "Failed to canonicalize image part path {:?}",
		    image_path
		))?;
	    top_image_path.get_or_insert_with(|| image_path_canon.clone());

	    let metadata_output =
		tokio::process::Command::new(&this.config.qemu.qemu_img_binary)
		.arg("info")
		.arg("--format=qcow2")
		.arg("--output=json")
		.arg("--")
		.arg(&image_path_canon)
		.output()
		.await
		.map_err(|e| anyhow::Error::from(e))
		.and_then(|output| {
		    // Ideally we'd want to use the nightly `exit_ok()` here:
		    if !output.status.success() {
			bail!("Running qemu-img failed with exit-code {:?}", output.status.code());
		    }

		    // Don't care about stderr:
		    Ok(output.stdout)
		})
		.with_context(|| format!("Failed to query image metadata for {:?}", image_path))?;

	    let metadata: QemuImgMetadata = serde_json::from_slice(&metadata_output)
		.with_context(|| format!("Failed to parse `qemu-img info` output for {:?}", image_path))?;

	    // Make sure that the image has just one child, and that child's
	    // filename is identical to the top-level filename. We do this as a
	    // precaution to images referencing other paths on our machine.
	    if metadata.children.len() != 1 {
		bail!("Image file {:?}  does not have exactly one child in qemu-img output", image_path);
	    }

	    let child = &metadata.children[0];
	    if child.filename != metadata.filename {
		bail!(
		    "Image file {:?}'s child filename ({:?}) diverges from \
		     main filename ({:?})",
		    image_path, &child.filename, &metadata.filename
		);
	    }

	    // Ensure that the image's advertised virtual size is identical to
	    // what we hold in the manifest:
	    let part_virtual_size: u64 = part_spec.attrs
		.get("org.tockos.treadmill.image.qemu_layered_v0.part-virtual-size")
		.ok_or_else(|| anyhow!("Image part {} missing `part-virtual-size` attribute", current_image))
		.and_then(|size_bytes_str| u64::from_str_radix(size_bytes_str, 10).with_context(|| format!("Parsing part {}'s part-virtual-size attribute as u64", current_image)))?;
	    if part_virtual_size != metadata.virtual_size {
		bail!(
		    "Image file {:?}'s virtual size ({} byte) diverges from \
		     manifest ({} byte)",
		    image_path, metadata.virtual_size, part_virtual_size,
		);
	    }

	    // The image must not be encrypted:
	    if metadata.encrypted.unwrap_or(false) {
		bail!(
		    "Image file {:?} is encrypted",
		    image_path,
		);
	    }

	    // If the image has a backing format, it must be "qcow2":
	    if let Some(fmt) = metadata.backing_filename_format {
		if fmt != "qcow2" {
		    bail!(
			"Image file {:?} can only have backing files of \
			 qcow2 format (actual: {})",
			image_path, fmt,
		    );
		}
	    }

	    // If the manifest has a `lower` attribute then this image must have
	    // a backing file attribute that's part of our image store, and the
	    // qcow2 image's backing file path must resolve to the same file as
	    // our image store lookup.
	    //
	    // If the manifest doesn't have a `lower` attribute, then the qcow2
	    // file may not have either `full_backing_filename` nor a
	    // `backing_filename`:
	    let part_lower_opt = part_spec.attrs
		.get("org.tockos.treadmill.image.qemu_layered_v0.lower");
	    if let Some(part_lower) = part_lower_opt {
		// We do have a lower part, look it up in the image store:
		let lower_part_spec = manifest.parts.get(part_lower)
		    .ok_or_else(|| anyhow!(
			"Image missing part {}, referenced by part {}",
			part_lower, current_image
		    ))?;

		let image_store_path = this.image_store_client
		    .part_path(&lower_part_spec.sha256_digest)
		    .await;

		let image_store_path_canon =
		    tokio::fs::canonicalize(&image_store_path)
		    .await
		    .with_context(|| format!(
			"Failed to canonicalize image part path {:?},
                             returned by image store lookup for part {}",
			image_store_path, part_lower,
		    ))?;

		let full_backing_filename_canon =
		    if let Some(f) = &metadata.full_backing_filename {
			tokio::fs::canonicalize(f)
			    .await
			    .with_context(|| format!(
				"Failed to canonicalize image part path {:?},
                                     referenced by qcow image file {}",
				f, current_image,
			    ))?
		    } else {
			bail!(
			    "Image {} supposed to reference part {}, but \
			     qemu-img references no backing file",
			    current_image, part_lower,
			);
		    };

		// Ensure that the canonical path representations for the
		// `image_path` and the path in the image metadata are
		// identical:
		if image_store_path_canon != full_backing_filename_canon {
		    bail!(
			"Image part {} maps this path in the image store: \
			 {:?}, but qcow2 of layer {} maps to {:?} as a \
			 backing file instead.",
			part_lower, image_store_path_canon, current_image,
			full_backing_filename_canon,
		    );
		}

		// All checks passed, continue with the next layer:
		current_image = part_lower;
	    } else {
		// Image is not supported to have any lower / backing layer:
		if metadata.backing_filename.is_some() || metadata.full_backing_filename.is_some() {
		    bail!(
			"Image part {} (file {:?}) does not have a \
			 lower-part specified by references a backing file",
			current_image, image_path_canon,
		    );
		}

		// We've reached the end of the image chain without error, break
		// out of the loop and return a success result.
		break Ok(());
	    }
	}
    }

    async fn start_job_cont(this: &Arc<Self>, job_id: Uuid) {
        // Obtain a reference to the job. It's possible for the job to have
        // transitioned from `ImageFetched` to any other state in between
        // `fetch_image` releasing its lock and this function executing, so if
        // the job is no longer alive or in the `ImageFetched` state, return.
        let job_opt = {
            // Do not hold onto the global job's lock:
            this.jobs.lock().await.get(&job_id).cloned()
        };

        // If the job is no longer alive, return:
        let job = match job_opt {
            Some(job) => job,
            None => {
                // This case should be pretty rare but does not indicate a bug
                // per se, so let's issue a debug message just in case:
                debug!(
                    "Job {:?} vanished before `start_job_cont` could acquire its lock",
                    job_id
                );
                return;
            }
        };

        // Acquire a lock on the job:
        let mut job_lg = job.lock().await;

        // We swap in a state of `Starting` temporarily to please the Rust
        // borrow checker. Before we leave this function, we must replace this
        // state again!
        //
        // If the job is in any state other than `ImageFetched`, swap back and
        // return immediately:
        let QemuSupervisorImageFetchedState {
            start_job_req,
            manifest,
        } = match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Starting) {
            QemuSupervisorJobState::ImageFetched(state) => state,
            prev_state => {
                // Swap the old state back:
                *job_lg = prev_state;

                // Same as above, let's issue a debug message:
                debug!(
                    "Job {:?} changed state before `start_job_cont` could \
			 aquire a lock: {:?}",
                    job_id, job,
                );

                return;
            }
        };

        // Inform the connector that we're now preparing for start:
        this.connector
            .update_job_state(
                start_job_req.job_id,
                connector::JobState::Starting {
                    stage: connector::JobStartingStage::Allocating,
                    status_message: None,
                },
            )
            .await;

	// Parse the manifest and sanity-check all image layers. We do this in
	// an async block such that we can use the `?` operator:
	let mut top_image_path = None;

	// Check whether the image is valid. If it's not, report to the
	// coordinator:
	if let Err(e) = parse_check_manifest_res {
	    // Image is invalid, report an error:
            this.connector
                .report_job_error(
                    start_job_req.job_id,
                    connector::JobError {
                        request_id: Some(start_job_req.request_id),
                        error_kind: connector::JobErrorKind::ImageInvalid,
                        description: format!(
			    "Validation of image {:?} failed: {:?}",
			    start_job_req.image_id, e
			),
                    },
                )
                .await;

            // No resources have been allocated (yet), so simply remove the
            // job and set its state to `Stopping`, in case anyone else has
            // a reference to it still.
            //
            // Safe to call, we don't hold a lock on `this.jobs`:
            this.remove_job(job_id).await;

            // Prevent other tasks from further advancing this job's state:
            *job_lg = QemuSupervisorJobState::Stopping;
	}

	// If we reach this point, we must have a "top" image part from which we
	// can create a new qcow overlay:
	let top_image_path = top_image_path.unwrap();
	println!("Top image path: {:?}", top_image_path);

	// Create a new working directory:

        // let (top_layer_id, top_layer_label, top_layer) = manifest
        //     .parts
        //     .iter()
        //     .filter_map(|(layer_name, _)| {
        //         layer_name
        //             .strip_prefix("org.tockos.treadmill.image.qemu_qcow2_layered.")
        //             .and_then(|layer_id_str| {
        //                 u64::from_str_radix(layer_id_str)
        //                     .map_err(|e| {
        //                         warn!("Image {:?}}: unable to parse id of image layer {}");
        //                         e
        //                     })
        //                     .ok()
        //             })
        //     })
        //     .max_by_key(|(layer_id, _, _)| layer_num);

        // Create a new thick-provisioned disk image with the specified
        // size. The virtual machine image can then be expanded.
        //
        // We want to make sure that the new CoW disk image layer is not smaller
        // than the one specified in the manifest for the image.

        unimplemented!();
        // // Make sure that we have access to the requested image (it's loaded
        // // into our local image cache). We don't support fetching an image yet,
        // // but we still check whether it exists, and otherwise return an error:

        // // Start a TCP control socket on the specified listen addr:
        // let control_socket = TcpControlSocket::new(
        //     msg.job_id,
        //     this.config.qemu.tcp_control_socket_listen_addr,
        //     this.clone(),
        // )
        // .await
        // .unwrap();

        // let qemu_proc = tokio::process::Command::new(&this.config.qemu.qemu_binary)
        //     .arg("--transport")
        //     .arg("auto_discover")
        //     .stdin(std::process::Stdio::null())
        //     .stdout(std::process::Stdio::inherit())
        //     .stderr(std::process::Stdio::inherit())
        //     .spawn()
        //     .unwrap();

        // // Job has been started, let the coordinator know:
        // this.connector
        //     .update_job_state(
        //         msg.job_id,
        //         connector::JobState::Starting {
        //             // Booting, but puppet has not yet reported "ready":
        //             stage: connector::JobStartingStage::Booting,
        //             status_message: None,
        //         },
        //     )
        //     .await;

        // // Mark the job as started:
        // *job_lg = QemuSupervisorJobState::Running(QemuSupervisorJobRunningState {
        //     control_socket,
        //     qemu_proc,
        // });
    }
}

#[async_trait]
impl connector::Supervisor for QemuSupervisor {
    async fn start_job(
        this: &Arc<Self>,
        start_job_req: connector::StartJobRequest,
    ) -> Result<(), connector::JobError> {
        // This method may be long-lived, but we should avoid performing
        // long-running, uninterruptible actions in here (as this will prevent
        // other events from being delivered). We're provided an &Arc<Self> to
        // be able to launch async tasks, while returning immediately. We only
        // perform sanity checks here and transition into other states that
        // perform potentially long-running actions.

        // Take a short-lived lock on the global jobs object to check that we're
        // not asked to double-start a job and whether we can fit another. If
        // everything's good, insert a job into the HashMap and return its `Arc`
        // reference. This way we don't hold the global lock for too long.
        //
        // We can't use a Rust scope, as we'll want to obtain a lock on the job
        // itself before releasing the global lock, such that we don't run the
        // risk of scheduling another action on this job when it's not yet
        // initialized fully.
        //
        // ============ GLOBAL `jobs` HASHMAP LOCK ACQUIRE ==================
        //
        let mut jobs_lg = this.jobs.lock().await;

        // Make sure that there's not another job with the same ID executing
        // currently. Even when we resume a job, it needs to have been
        // stopped first:
        if jobs_lg.get(&start_job_req.job_id).is_some() {
            return Err(connector::JobError {
                request_id: Some(start_job_req.request_id),
                error_kind: connector::JobErrorKind::AlreadyRunning,
                description: format!(
                    "Job {:?} is already running and cannot be started again.",
                    start_job_req.job_id
                ),
            });
        }

        // Don't start more jobs than we're allowed to. Currently, the QEMU
        // supervisor only supports one job at a time (otherwise we'd need to
        // reason about IP address assignment from a pool, customizable
        // parameters for each instance, etc.).
        if jobs_lg.len() > 1 {
            return Err(connector::JobError {
                request_id: Some(start_job_req.request_id),
                error_kind: connector::JobErrorKind::MaxConcurrentJobs,
                description: format!(
                    "Supervisor {:?} cannot start any more concurrent jobs (running {}, max 1).",
                    this.config.base.supervisor_id,
                    jobs_lg.len(),
                ),
            });
        }

        // We're good to create this job, create it in the `Starting` state:
        let job = Arc::new(Mutex::new(QemuSupervisorJobState::Starting));

        // Acquire a lock on the job. No one else has a reference yet, so this
        // should succeed immediately:
        let mut job_lg = job.lock().await;

        // Insert a clone of the Arc into the HashMap:
        jobs_lg.insert(start_job_req.job_id, job.clone());

        // Release the global lock here:
        std::mem::drop(jobs_lg);
        //
        // ========== GLOBAL `jobs` HASHMAP LOCK RELEASED ======================

        // The job was inserted into the `jobs` HashMap and initialized as
        // `Starting`, let the coordinator know:
        this.connector
            .update_job_state(
                start_job_req.job_id,
                connector::JobState::Starting {
                    // Generic starting stage. We don't fetch, allocate or provision any
                    // resources right now, so report a generic state instead:
                    stage: connector::JobStartingStage::Starting,
                    status_message: None,
                },
            )
            .await;

        // Fetch the requested image.
        //
        // We avoid locking the job's state for long periods of time, e.g., to
        // allow cancelling it before the image has been fully fetched.  Thus,
        // we poll the image supervisor repeatedly, but don't hold onto the
        // job's lock in between these polling operations.

        // Put the job into the `FetchingImage` state:
        let job_id = start_job_req.job_id; // Copy required below
        *job_lg = QemuSupervisorJobState::FetchingImage(QemuSupervisorFetchingImageState {
            start_job_req,
            // Will be set to Some(...) when an asynchronous fetch operation is
            // kicked off:
            poll_task: None,
        });

        // Release our lock on the job and hand over to the fetch image method:
        std::mem::drop(job_lg);
        Self::fetch_image(this, job_id).await;

        Ok(())
    }

    async fn stop_job(
        this: &Arc<Self>,
        msg: connector::StopJobRequest,
    ) -> Result<(), connector::JobError> {
        // We do not immediately remove the job from the global jobs HashMap, as
        // we want to deallocate all resources before a job with an identical ID
        // can be resumed again. Thus, first transition it into a `Stopping`
        // state and return a reference to it. We take ownership of the old job
        // state and destruct it.

        // Get a reference to this job by an emphemeral lock on `jobs` HashMap:
        let job: Arc<Mutex<QemuSupervisorJobState>> = {
            this.jobs
                .lock()
                .await
                .get(&msg.job_id)
                .cloned()
                .ok_or(connector::JobError {
                    request_id: Some(msg.request_id),
                    error_kind: connector::JobErrorKind::JobNotFound,
                    description: format!("Job {:?} not found, cannot stop.", msg.job_id),
                })?
        };

        let mut job_lg = job.lock().await;

        enum StopJobPrevState {
            FetchingImage(QemuSupervisorFetchingImageState),
            ImageFetched(QemuSupervisorImageFetchedState),
            Running(QemuSupervisorJobRunningState),
        }

        // Make sure the job is in a state in which we can stop it. If so, place
        // it into the `Stopping` state.
        let prev_job_state = match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Stopping)
        {
            // Stoppable states:
            QemuSupervisorJobState::FetchingImage(s) => StopJobPrevState::FetchingImage(s),
            QemuSupervisorJobState::ImageFetched(s) => StopJobPrevState::ImageFetched(s),
            QemuSupervisorJobState::Running(s) => StopJobPrevState::Running(s),

            prev_state @ QemuSupervisorJobState::Starting => {
                // Put back the previous state:
                *job_lg = prev_state;

                // We must never be able to acquire a lock over a job in
                // this state. The job will atomically transition from
                // `Starting` to some other state in the implementation of
                // `start_job`. This state is just a placeholder in the
                // global jobs map, such that no other job with the same ID
                // can be started:
                unreachable!("Job must not be in `Starting` state!");
            }

            prev_state @ QemuSupervisorJobState::Stopping => {
                // Put back the previous state:
                *job_lg = prev_state;

                return Err(connector::JobError {
                    request_id: Some(msg.request_id),
                    error_kind: connector::JobErrorKind::AlreadyStopping,
                    description: format!("Job {:?} is already stopping.", msg.job_id),
                });
            }
        };

        // Job is stopping, let the coordinator know:
        this.connector
            .update_job_state(
                msg.job_id,
                connector::JobState::Stopping {
                    status_message: None,
                },
            )
            .await;

        // Perform actions depending on the previous job state:
        match prev_job_state {
            StopJobPrevState::FetchingImage(QemuSupervisorFetchingImageState {
                start_job_req: _,
                poll_task,
            }) => {
                // Make sure that we don't have another poll task
                // scheduled. This is potentially racy, where this abort() call
                // can come right after an invocation of `Self::fetch_image` has
                // been scheduled. However, that function will not do anything,
                // given the job state is now `Stopping` instead of
                // `FetchingImage`:
                if let Some(handle) = poll_task {
                    handle.abort();
                }
            }

            StopJobPrevState::ImageFetched(QemuSupervisorImageFetchedState {
		start_job_req: _,
		manifest: _
	    }) => {
                // Nothing to do, the only time we're seeing this state is in
                // between fetching image, and actually starting the job. Given
                // that we've acquired the lock between those two functions, we
                // don't need to deallocate, shut down or abort anything.
            }

            StopJobPrevState::Running(QemuSupervisorJobRunningState {
                control_socket,
                mut qemu_proc,
            }) => {
                // TODO: kindly request the puppet to shut down. Here we simply
                // force it to quit (by using a SIGKILL). This is not nice.
                qemu_proc.kill().await.unwrap();

                // Shut down the control socket server:
                control_socket.shutdown().await.unwrap();
            }
        }

        // Job has been stopped, let the coordinator know:
        this.connector
            .update_job_state(
                msg.job_id,
                connector::JobState::Finished {
                    status_message: None,
                },
            )
            .await;

        // Finally, remove the job from the jobs HashMap. Eventually, all other
        // `Arc` references (including the one we hold) will get dropped.
        assert!(this.jobs.lock().await.remove(&msg.job_id).is_some());

        Ok(())
    }
}

#[async_trait]
impl control_socket::Supervisor for QemuSupervisor {
    async fn ssh_keys(&self, tgt_job_id: Uuid) -> Option<Vec<String>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // We don't actually store any SSH keys for the QemuSupervisor
                // job, so just return an empty set:
                QemuSupervisorJobState::Running { .. } => Some(vec![]),

                // Only respond to host / puppet requests when the job is marked
                // as "running":
                state => {
                    warn!(
                        "Received puppet SSH keys request for job {:?} in invalid state: {}",
                        tgt_job_id,
                        state.state_name()
                    );
                    None
                }
            },

            // Job not found:
            None => {
                warn!(
                    "Received puppet SSH keys request for non-existant job: {:?}",
                    tgt_job_id
                );
                None
            }
        }
    }

    async fn network_config(
        &self,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::NetworkConfig> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running, respond with its assigned hostname:
                QemuSupervisorJobState::Running { .. } => {
                    let hostname = format!("job-{}", format!("{}", tgt_job_id).split_at(10).0);
                    Some(treadmill_rs::api::supervisor_puppet::NetworkConfig {
                        hostname,
                        // QemuSupervisor, don't supply a network interface to configure:
                        interface: None,
                        ipv4: None,
                        ipv6: None,
                    })
                }

                // Only respond to host / puppet requests when the job is marked
                // as "running":
                state => {
                    warn!(
                        "Received puppet SSH keys request for job {:?} in invalid state: {}",
                        tgt_job_id,
                        state.state_name()
                    );
                    None
                }
            },

            // Job not found:
            None => {
                warn!(
                    "Received puppet network config request for non-existant job: {:?}",
                    tgt_job_id
                );
                None
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    use simplelog::{self, ColorChoice, LevelFilter, TermLogger, TerminalMode};
    use treadmill_rs::connector::SupervisorConnector;

    TermLogger::init(
        LevelFilter::Debug,
        simplelog::ConfigBuilder::new()
            .set_target_level(LevelFilter::Debug)
            .build(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();
    info!("Treadmill Qemu Supervisor, Hello World!");

    let args = QemuSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: QemuSupervisorConfig = toml::from_str(&config_str).unwrap();

    let image_store =
        image_store_client::ImageStoreClient::new("http://localhost:4242".to_string())
            .await
            .unwrap()
            .into_local("/shared_mount")
            .await
            .unwrap();

    match config.base.coord_connector {
        SupervisorCoordConnector::CliConnector => {
            let cli_connector_config = config.cli_connector.clone().ok_or(anyhow!(
                "Requested CliConnector, but `cli_connector` config not present."
            ))?;

            // Both the supervisor and connectors have references to each other,
            // so we break the cyclic dependency with an initially unoccupied
            // weak Arc reference:
            let mut connector_opt = None;

            let qemu_supervisor = {
                // Shadow, to avoid moving the variable:
                let connector_opt = &mut connector_opt;
                Arc::new_cyclic(move |weak_supervisor| {
                    let connector = Arc::new(tml_cli_connector::CliConnector::new(
                        config.base.supervisor_id,
                        cli_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    QemuSupervisor::new(connector, image_store, args, config)
                })
            };

            let connector = connector_opt.take().unwrap();

            connector.run().await;

            // Must drop qemu_supervisor reference _after_ connector.run(), as
            // that'll upgrade its Weak into an Arc. Otherwise we're dropping
            // the only reference to it:
            std::mem::drop(qemu_supervisor);

            Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
}
