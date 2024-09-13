use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_recursion::async_recursion;
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{event, instrument, Level};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::JobInitSpec;
use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::image;
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use tml_tcp_control_socket_server::TcpControlSocket;

mod image_store_client;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
struct QemuImgChildMetadataInfo {
    // We're only interested in the filename here, to make sure that the
    // image has only one child node, and that node coincides with the
    // file that we're operating on.
    filename: PathBuf,

    // Include this field just to make sure that we don't have
    // any recursive children:
    children: Vec<QemuImgChildMetadata>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
struct QemuImgChildMetadata {
    info: QemuImgChildMetadataInfo,
}

#[derive(Deserialize, Debug, Clone)]
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

    /// Directory to keep state:
    state_dir: PathBuf,

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
    qemu_args: Vec<String>,

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

    start_script: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configurations for individual connector implementations. All are
    /// optional, and not all of them have to be supported:
    cli_connector: Option<tml_cli_connector::CliConnectorConfig>,

    ws_connector: Option<tml_ws_connector::WsConnectorConfig>,

    image_store: image_store_client::LocalImageStoreConfig,

    qemu: QemuConfig,
}

#[derive(Debug)]
pub struct QemuSupervisorFetchingImageState {
    start_job_req: connector::StartJobMessage,
    image_id: image::manifest::ImageId,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
pub struct QemuSupervisorImageFetchedState {
    start_job_req: connector::StartJobMessage,
    image_id: image::manifest::ImageId,
    manifest: image::manifest::ImageManifest,
}

#[derive(Debug)]
pub struct QemuSupervisorJobRunningState {
    start_job_req: connector::StartJobMessage,

    /// The qemu process handle:
    qemu_proc: tokio::process::Child,

    /// The monitoring task handle for the QEMU process.
    qemu_monitor_task: tokio::task::JoinHandle<()>,

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

#[derive(Debug)]
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

    _args: QemuSupervisorArgs,
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
            _args: args,
            config,
        }
    }

    #[instrument(skip(self))]
    async fn remove_job(&self, job_id: Uuid) -> Option<Arc<Mutex<QemuSupervisorJobState>>> {
        event!(Level::DEBUG, "Removing job {job_id} from jobs map");
        let job = self.jobs.lock().await.remove(&job_id);

        if let Some(job_state) = &job {
            let mut job_lg = job_state.lock().await;

            // Perform any additional cleanup here if necessary
            match &*job_lg {
                QemuSupervisorJobState::Running(state) => {
                    // Cancel the monitoring task if still running
                    state.qemu_monitor_task.abort();
                }
                _ => {}
            }
        }

        job
    }

    #[instrument(skip(this))]
    #[async_recursion]
    async fn fetch_image(this: &Arc<Self>, job_id: Uuid, iteration: usize) {
        event!(Level::DEBUG, "Entering fetch_image");

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
            Some(job) => {
                event!(Level::TRACE, "Retrieved job object reference");
                job
            }
            None => {
                // This case should be pretty rare but does not indicate a bug
                // per se, so let's issue a debug message just in case:
                event!(
                    Level::DEBUG,
                    "Job {job_id} vanished across invocations of `fetch_image`",
                );
                return;
            }
        };

        // Acquire a lock on the job:
        let mut job_lg = job.lock().await;
        event!(Level::DEBUG, "Acquired job object lock");

        // We swap in a state of `Starting` temporarily to please the Rust
        // borrow checker. Before we leave this function, we must replace this
        // state again!
        //
        // If the job is in any state other than `FetchingImage`, swap back and
        // return immediately:
        let fetching_image_state = match std::mem::replace(
            &mut *job_lg,
            QemuSupervisorJobState::Starting,
        ) {
            QemuSupervisorJobState::FetchingImage(state) => state,
            prev_state => {
                // Swap the old state back:
                *job_lg = prev_state;

                // Same as above, let's issue a debug message:
                event!(
			Level::DEBUG,
                        "Job not in FetchingImage state (likely changed state across invocations of `fetch_image`): {job:?}. Returning.",
                    );

                return;
            }
        };

        // Check whether the image store already holds this image. If it does,
        // we can pass the manifest to the `start_job_cont` function. Otherwise,
        // (continue) polling:
        let fetch_endpoints = vec![];
        event!(Level::TRACE, ?fetch_endpoints, image_id = ?fetching_image_state.image_id, "Requesting that image_store_client fetch image");
        match this
            .image_store_client
            .fetch_image(
                // TODO: pass remote store endpoints:
                fetch_endpoints,
                fetching_image_state.image_id,
            )
            .await
        {
            Ok(image_store_client::FetchImageStatus::Present) => {
                event!(
                    Level::DEBUG,
                    "image_store_client.fetch_image indicates that the image is present"
                );
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

                let manifest = match this
                    .image_store_client
                    .image_manifest(fetching_image_state.image_id)
                    .await
                {
                    Ok(manifest) => manifest,
                    Err(manifest_err) => {
                        // We could not retrieve the manifest, despite the image
                        // being present. Don't attempt to recover from this
                        // error and mark the job as failed:
                        event!(Level::WARN, ?manifest_err, "Failed to retrieve image manifest, reporting job error: {manifest_err:?}");
                        this.connector
                            .report_job_error(
                                fetching_image_state.start_job_req.job_id,
                                connector::JobError {
                                    error_kind: connector::JobErrorKind::InternalError,
                                    description: format!(
                                        "Cannot retrieve image manifest of {:?}: {:?}",
                                        fetching_image_state.image_id, manifest_err,
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
                event!(
                    Level::DEBUG,
                    ?manifest,
                    "Transitioning job into ImageFetched state"
                );
                *job_lg = QemuSupervisorJobState::ImageFetched(QemuSupervisorImageFetchedState {
                    start_job_req: fetching_image_state.start_job_req,
                    image_id: fetching_image_state.image_id,
                    manifest,
                });

                // Release the lock before continuing to start, otherwise this
                // will deadlock:
                std::mem::drop(job_lg);
                Self::start_job_cont(this, job_id).await
            }

            Ok(image_store_client::FetchImageStatus::InProgress(msg)) => {
                event!(
                    Level::DEBUG,
                    "Image manifest fetch in progress, polling: {msg:?}"
                );

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
                            Self::fetch_image(&poll_task_this, job_id, iteration + 1).await
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
                event!(Level::DEBUG, "Transitioning job into FetchingImage state");
                *job_lg = QemuSupervisorJobState::FetchingImage(QemuSupervisorFetchingImageState {
                    poll_task: Some(poll_task),
                    ..fetching_image_state
                });
            }

            Err(e) => {
                // We could not fetch the image, report an error:
                event!(Level::DEBUG, fetch_image_error = ?e, "Failed to fetch image, reporting job error: {e:?}");
                this.connector
                    .report_job_error(
                        fetching_image_state.start_job_req.job_id,
                        connector::JobError {
                            error_kind: connector::JobErrorKind::InternalError,
                            description: format!(
                                "Failed to fetch image {:?}: {:?}",
                                fetching_image_state.image_id, e
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
                event!(Level::DEBUG, "Transitioning job into Stopping state");
                *job_lg = QemuSupervisorJobState::Stopping;
            }
        }
    }

    #[instrument(skip(self, manifest), err(Debug, level = Level::WARN))]
    async fn start_job_parse_manifest_check_images(
        &self,
        manifest: &image::manifest::ImageManifest,
    ) -> Result<(PathBuf, QemuImgMetadata)> {
        event!(
            Level::DEBUG,
            ?manifest,
            "Checking & extracting head-image layer from image manifest"
        );

        // Retrieve the "top-most" layer in the image manifest, from which we'll
        // create our "working" disk image overlay:
        let top_layer_label = manifest
            .attrs
            .get("org.tockos.treadmill.image.qemu_layered_v0.head")
            .ok_or(anyhow!("Image does not have a head layer defined"))?;

        // Now step through each image layer, making sure that it exists and (if
        // it has a predecessor defined) it points to that predecessor. We don't
        // want images to be able to reference arbitrary paths in our filesystem
        // as underlying layers.
        //
        // We use qemu-img to retrieve the (partial) metadata of an image.

        // Now, for every image in the chain, parse the metadata and make sure
        // that the image file:
        //
        // - Exists,
        // - Matches its specification in the manifest, and
        // - Only references its defined predecessor.
        let mut top_image = None;
        let mut current_image = top_layer_label;
        loop {
            // Acquire the manifest metadata for this image blob:
            let blob_spec = manifest
                .blobs
                .get(current_image)
                .ok_or_else(|| anyhow!("Image missing blob {}", current_image))?;

            // Try to read image attributes with `qemu-img`:
            let image_path = self
                .image_store_client
                .blob_path(&blob_spec.sha256_digest)
                .await;

            let image_path_canon =
                tokio::fs::canonicalize(&image_path)
                    .await
                    .with_context(|| {
                        format!("Failed to canonicalize image blob path {:?}", image_path)
                    })?;

            event!(Level::DEBUG, qemu_img_binary = ?self.config.qemu.qemu_img_binary, file = ?image_path_canon, "Retrieving qcow2 file metadata");
            let metadata_output = tokio::process::Command::new(&self.config.qemu.qemu_img_binary)
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
                        bail!(
                            "Running qemu-img failed with exit-code {:?}",
                            output.status.code()
                        );
                    }

                    // Don't care about stderr:
                    Ok(output.stdout)
                })
                .with_context(|| format!("Failed to query image metadata for {:?}", image_path))?;

            let metadata: QemuImgMetadata =
                serde_json::from_slice(&metadata_output).with_context(|| {
                    format!(
                        "Failed to parse `qemu-img info` output for {:?}: {:?}",
                        image_path,
                        String::from_utf8_lossy(&metadata_output),
                    )
                })?;

            top_image.get_or_insert_with(|| {
		event!(Level::DEBUG, file = ?image_path_canon, ?metadata, "Setting image file as top-level");
		(image_path_canon.clone(), metadata.clone())
	    });

            // Make sure that the image has just one child, and that child's
            // filename is identical to the top-level filename. We do this as a
            // precaution to images referencing other paths on our machine.
            if metadata.children.len() != 1 || metadata.children[0].info.children.len() != 0 {
                bail!(
                    "Image file {:?} does not have exactly one (recursive) child in qemu-img output",
                    image_path
                );
            }

            let child = &metadata.children[0];
            if child.info.filename != metadata.filename {
                bail!(
                    "Image file {:?}'s child filename ({:?}) diverges from \
		     main filename ({:?})",
                    image_path,
                    &child.info.filename,
                    &metadata.filename
                );
            }

            // Ensure that the image's advertised virtual size is identical to
            // what we hold in the manifest:
            let blob_virtual_size: u64 = blob_spec
                .attrs
                .get("org.tockos.treadmill.image.qemu_layered_v0.blob-virtual-size")
                .ok_or_else(|| {
                    anyhow!(
                        "Image blob {} missing `blob-virtual-size` attribute",
                        current_image
                    )
                })
                .and_then(|size_bytes_str| {
                    u64::from_str_radix(size_bytes_str, 10).with_context(|| {
                        format!(
                            "Parsing blob {}'s blob-virtual-size attribute as u64",
                            current_image
                        )
                    })
                })?;
            if blob_virtual_size != metadata.virtual_size {
                bail!(
                    "Image file {:?}'s virtual size ({} byte) diverges from \
		     manifest ({} byte)",
                    image_path,
                    metadata.virtual_size,
                    blob_virtual_size,
                );
            }

            // The image must not be encrypted:
            if metadata.encrypted.unwrap_or(false) {
                bail!("Image file {:?} is encrypted", image_path,);
            }

            // If the image has a backing format, it must be "qcow2":
            if let Some(fmt) = metadata.backing_filename_format {
                if fmt != "qcow2" {
                    bail!(
                        "Image file {:?} can only have backing files of \
			 qcow2 format (actual: {})",
                        image_path,
                        fmt,
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
            let blob_lower_opt = blob_spec
                .attrs
                .get("org.tockos.treadmill.image.qemu_layered_v0.lower");
            if let Some(blob_lower) = blob_lower_opt {
                // We do have a lower blob, look it up in the image store:
                let lower_blob_spec = manifest.blobs.get(blob_lower).ok_or_else(|| {
                    anyhow!(
                        "Image missing blob {}, referenced by blob {}",
                        blob_lower,
                        current_image
                    )
                })?;

                let image_store_path = self
                    .image_store_client
                    .blob_path(&lower_blob_spec.sha256_digest)
                    .await;

                let image_store_path_canon = tokio::fs::canonicalize(&image_store_path)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to canonicalize image blob path {:?},
                             returned by image store lookup for blob {}",
                            image_store_path, blob_lower,
                        )
                    })?;

                let full_backing_filename_canon = if let Some(f) = &metadata.full_backing_filename {
                    tokio::fs::canonicalize(f).await.with_context(|| {
                        format!(
                            "Failed to canonicalize image blob path {:?},
                                     referenced by qcow image file {}",
                            f, current_image,
                        )
                    })?
                } else {
                    bail!(
                        "Image {} supposed to reference blob {}, but \
			     qemu-img references no backing file",
                        current_image,
                        blob_lower,
                    );
                };

                // Ensure that the canonical path representations for the
                // `image_path` and the path in the image metadata are
                // identical:
                if image_store_path_canon != full_backing_filename_canon {
                    bail!(
                        "Image blob {} maps this path in the image store: \
			 {:?}, but qcow2 of layer {} maps to {:?} as a \
			 backing file instead.",
                        blob_lower,
                        image_store_path_canon,
                        current_image,
                        full_backing_filename_canon,
                    );
                }

                // All checks passed, continue with the next layer:
                current_image = blob_lower;
            } else {
                // Image is not supported to have any lower / backing layer:
                if metadata.backing_filename.is_some() || metadata.full_backing_filename.is_some() {
                    bail!(
                        "Image blob {} (file {:?}) does not have a \
			 lower-blob specified by references a backing file",
                        current_image,
                        image_path_canon,
                    );
                }

                // We've reached the end of the image chain without error, break
                // out of the loop.
                break;
            }
        }

        // If we reach this point, we must have a path to the top
        // layer of the image. Return it:
        Ok(top_image.unwrap())
    }

    #[instrument(skip(self, start_job_req, top_image_qemu_info), err(Debug, level = Level::WARN))]
    async fn start_job_parse_manifest_allocate_disk(
        &self,
        start_job_req: &connector::StartJobMessage,
        top_image_path: &Path,
        top_image_qemu_info: &QemuImgMetadata,
    ) -> Result<(PathBuf, PathBuf), connector::JobError> {
        let jobs_dir = self.config.qemu.state_dir.join("jobs");
        let job_dir = jobs_dir.join(&start_job_req.job_id.to_string());

        // Ensure that the state/jobs directory (and all recursive parents)
        // exists and create a new working directory for this job:
        event!(Level::DEBUG, ?job_dir, "Creating job state dir");

        tokio::fs::create_dir_all(&jobs_dir)
            .await
            .map_err(|io_err| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Failed to create state dir for job {}: {:?}",
                    start_job_req.job_id, io_err,
                ),
            })?;

        match tokio::fs::create_dir(&job_dir).await {
            Ok(()) => (),

            Err(io_err) if io_err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(connector::JobError {
                    error_kind: connector::JobErrorKind::JobAlreadyExists,
                    description: format!(
                        "A job with {:?} was previously started on this supervisor",
                        start_job_req.job_id,
                    ),
                });
            }

            Err(io_err) => {
                return Err(connector::JobError {
                    error_kind: connector::JobErrorKind::InternalError,
                    description: format!(
                        "Failed to create state dir for job {}: {:?}",
                        start_job_req.job_id, io_err,
                    ),
                });
            }
        };

        // Create a new thin-provisioned disk image with the specified size. The
        // virtual machine image can then be expanded.
        //
        // We want to make sure that the new CoW disk image layer is not smaller
        // than the one specified in the manifest for the image.
        if top_image_qemu_info.virtual_size > self.config.qemu.working_disk_max_bytes {
            return Err(connector::JobError {
                error_kind: connector::JobErrorKind::JobAlreadyExists,
                description: format!(
                    "A job with {:?} was previously started on this supervisor",
                    start_job_req.job_id,
                ),
            });
        }

        // Create the disk, based on the top image layer:
        let disk_image_file = job_dir.join("disk.qcow2");
        event!(Level::DEBUG, backing_image = ?top_image_path, ?disk_image_file, virtual_size_bytes = self.config.qemu.working_disk_max_bytes, "Creating job disk image");
        tokio::process::Command::new(&self.config.qemu.qemu_img_binary)
            .arg("create")
            // Image format:
            .arg("-f")
            .arg("qcow2")
            // Backing file format:
            .arg("-F")
            .arg("qcow2")
            // Backing file:
            .arg("-b")
            .arg(&top_image_path)
            // New image file:
            .arg(&disk_image_file)
            // New image size (in bytes, without suffix):
            .arg(&self.config.qemu.working_disk_max_bytes.to_string())
            .output()
            .await
            .map_err(|e| anyhow::Error::from(e))
            .and_then(|output| {
                // Ideally we'd want to use the nightly `exit_ok()` here:
                if !output.status.success() {
                    bail!(
                        "Running qemu-img failed with exit-code {:?}, stdout: {:?}, stderr: {:?}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }

                // Don't care about stdout or stderr in the success case:
                Ok(())
            })
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Failed to allocate disk image for job {:?}: {:?}",
                    start_job_req.job_id, e,
                ),
            })?;

        Ok((job_dir, disk_image_file))
    }

    #[instrument(skip(this))]
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
            Some(job) => {
                event!(Level::TRACE, "Retrieved job object reference");
                job
            }
            None => {
                // This case should be pretty rare but does not indicate a bug
                // per se, so let's issue a debug message just in case:
                event!(
                    Level::DEBUG,
                    "Job vanished before `start_job_cont` could acquire its lock",
                );
                return;
            }
        };

        // Acquire a lock on the job:
        let mut job_lg = job.lock().await;
        event!(Level::DEBUG, "Acquired job object lock");

        // We swap in a state of `Starting` temporarily to please the Rust
        // borrow checker. Before we leave this function, we must replace this
        // state again!
        //
        // If the job is in any state other than `ImageFetched`, swap back and
        // return immediately:
        let QemuSupervisorImageFetchedState {
            start_job_req,
            image_id,
            manifest,
        } = match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Starting) {
            QemuSupervisorJobState::ImageFetched(state) => state,
            prev_state => {
                // Swap the old state back:
                *job_lg = prev_state;

                // Same as above, let's issue a debug message:
                event!(
		    Level::DEBUG,
                    "Job not in ImageFetched state (likely changed state before `start_job_cont` could aquire a lock): {job:?}",
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
        let (top_image_path, top_image_qemu_info) =
            match this.start_job_parse_manifest_check_images(&manifest).await {
                Ok(v) => v,

                Err(e) => {
                    // Image is invalid, report an error:
                    this.connector
                        .report_job_error(
                            start_job_req.job_id,
                            connector::JobError {
                                error_kind: connector::JobErrorKind::ImageInvalid,
                                description: format!(
                                    "Validation of image {:?} failed: {:?}",
                                    image_id, e
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

                    return;
                }
            };

        // Allocate the job's disk:
        let (job_workdir, job_disk_image_path) = match this
            .start_job_parse_manifest_allocate_disk(
                &start_job_req,
                &top_image_path,
                &top_image_qemu_info,
            )
            .await
        {
            Ok(v) => v,

            Err(job_error) => {
                // Image is invalid, report an error:
                this.connector
                    .report_job_error(start_job_req.job_id, job_error)
                    .await;

                // No resources have been allocated (yet), so simply remove the
                // job and set its state to `Stopping`, in case anyone else has
                // a reference to it still.
                //
                // Safe to call, we don't hold a lock on `this.jobs`:
                this.remove_job(job_id).await;

                // Prevent other tasks from further advancing this job's state:
                *job_lg = QemuSupervisorJobState::Stopping;

                return;
            }
        };

        // Start script environment variables / QEMU command line
        // parameter template strings:
        event!(Level::DEBUG, "Templating QEMU argument substitutions");
        let mut qemu_arg_substs: HashMap<String, String> = HashMap::new();
        assert!(qemu_arg_substs
            .insert("job_id".to_string(), start_job_req.job_id.to_string())
            .is_none());
        assert!(qemu_arg_substs
            .insert("job_workdir".to_string(), job_workdir.display().to_string())
            .is_none());
        assert!(qemu_arg_substs
            .insert(
                "main_disk_image".to_string(),
                job_disk_image_path.display().to_string()
            )
            .is_none());

        if let Some(ref start_script) = this.config.qemu.start_script {
            event!(Level::DEBUG, ?start_script, "Executing start script");
            let start_script_res = tokio::process::Command::new(start_script)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .envs(
                    qemu_arg_substs
                        .iter()
                        .map(|(k, v)| (format!("TML_{}", k.to_uppercase()), v)),
                )
                .output()
                .await
                .unwrap(); // TODO: remove panic!

            assert!(start_script_res.status.success(), "Start script failed!");

            if let Ok(stdout) = std::str::from_utf8(&start_script_res.stdout) {
                for line in stdout.lines() {
                    if let Some(key_value) = line.strip_prefix("tml-set-variable:") {
                        if let Some((key, value)) = key_value.split_once('=') {
                            event!(
                                Level::DEBUG,
                                key,
                                value,
                                "Adding variable {key} to QEMU arg substs",
                            );
                            qemu_arg_substs.insert(key.to_string(), value.to_string());
                        } else {
                            event!(
                                Level::WARN,
                                command = line,
                                "Malformed tml-set-variable command"
                            );
                        }
                    }
                }
            } else {
                event!(
                    Level::WARN,
                    stdout = %String::from_utf8_lossy(&start_script_res.stdout),
                    "Start script produced non-UTF8 characters on standard output, refusing to interpret",
                );
            }
        }

        let qemu_args = match this
            .config
            .qemu
            .qemu_args
            .iter()
            .map(|argstr| strfmt::strfmt(argstr, &qemu_arg_substs))
            .collect::<Result<Vec<String>, strfmt::FmtError>>()
        {
            Ok(templated) => templated,

            Err(format_error) => {
                // Image is invalid, report an error:
                this.connector
                    .report_job_error(
                        start_job_req.job_id,
                        connector::JobError {
                            error_kind: connector::JobErrorKind::InternalError,
                            description: format!(
                                "Failed to generate QEMU command line arguments: {:?}",
                                format_error,
                            ),
                        },
                    )
                    .await;

                // TODO: remove job resources here.

                // Safe to call, we don't hold a lock on `this.jobs`:
                this.remove_job(job_id).await;

                // Prevent other tasks from further advancing this job's state:
                *job_lg = QemuSupervisorJobState::Stopping;

                return;
            }
        };

        // Start a TCP control socket on the specified listen addr:
        let control_socket = TcpControlSocket::new(
            start_job_req.job_id,
            this.config.qemu.tcp_control_socket_listen_addr,
            this.clone(),
        )
        .await
        .unwrap();

        event!(Level::INFO, qemu_binary = ?this.config.qemu.qemu_binary, ?qemu_args, "Launching QEMU process");
        let mut qemu_proc = tokio::process::Command::new(&this.config.qemu.qemu_binary)
            .args(&qemu_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();

        // Wrap qemu_proc in an Arc<Mutex<_>> for shared access
        let qemu_proc = Arc::new(Mutex::new(qemu_proc));

        // Clone references for the monitoring task
        let qemu_proc_clone = qemu_proc.clone();
        let supervisor_clone = this.clone();
        let job_id_clone = job_id;

        // Spawn the monitoring task
        let qemu_monitor_task = tokio::spawn(async move {
            if let Err(e) = async {
                let mut qemu_proc = qemu_proc_clone.lock().await;
                let exit_status = qemu_proc.wait().await;
                supervisor_clone
                    .handle_qemu_exit(job_id_clone, exit_status)
                    .await;
                Ok::<(), Box<dyn std::error::Error>>(())
            }
            .await
            {
                event!(
                    Level::ERROR,
                    ?e,
                    "Error in QEMU monitor task for job {job_id_clone}"
                );
            }
        });

        // Job has been started, let the coordinator know:
        this.connector
            .update_job_state(
                start_job_req.job_id,
                connector::JobState::Starting {
                    stage: connector::JobStartingStage::Booting,
                    status_message: None,
                },
            )
            .await;

        // Store the qemu_proc and qemu_monitor_task in the job state
        *job_lg = QemuSupervisorJobState::Running(QemuSupervisorJobRunningState {
            start_job_req,
            control_socket,
            qemu_proc,
            qemu_monitor_task,
        });
    }

    async fn handle_qemu_exit(
        &self,
        job_id: Uuid,
        exit_status: Result<ExitStatus, std::io::Error>,
    ) {
        // Log the exit status
        event!(
            Level::INFO,
            ?exit_status,
            "QEMU process for job {job_id} exited"
        );

        // Acquire the job from the jobs map
        if let Some(job) = self.jobs.lock().await.get(&job_id).cloned() {
            let mut job_lg = job.lock().await;

            // Check if the job is in the Running state
            if let QemuSupervisorJobState::Running(_) = *job_lg {
                // Transition the job to the Stopping state
                *job_lg = QemuSupervisorJobState::Stopping;

                // Send a message via the connector
                self.connector
                    .update_job_state(
                        job_id,
                        connector::JobState::Finished {
                            status_message: Some("QEMU process exited".to_string()),
                        },
                    )
                    .await;

                // Remove the job from the jobs map
                self.remove_job(job_id).await;
            } else {
                // Job is not in Running state; possibly already stopped
                event!(
                    Level::WARN,
                    "Job {job_id} is not in Running state during QEMU exit handling"
                );
            }
        } else {
            // Job not found in the jobs map
            event!(
                Level::WARN,
                "Job {job_id} not found in jobs map during QEMU exit handling"
            );
        }
    }
}

#[async_trait]
impl connector::Supervisor for QemuSupervisor {
    #[instrument(skip(this, start_job_req), fields(job_id = ?start_job_req.job_id), err(Debug, level = Level::WARN))]
    async fn start_job(
        this: &Arc<Self>,
        start_job_req: connector::StartJobMessage,
    ) -> Result<(), connector::JobError> {
        event!(Level::INFO, ?start_job_req);

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

        let image_id = match start_job_req.init_spec {
            JobInitSpec::Image { image_id } => image_id,
            unsupported_init_spec => {
                unimplemented!("Unsupported init spec: {:?}", unsupported_init_spec)
            }
        };

        // Put the job into the `FetchingImage` state:
        let job_id = start_job_req.job_id; // Copy required below
        *job_lg = QemuSupervisorJobState::FetchingImage(QemuSupervisorFetchingImageState {
            start_job_req,
            image_id,
            // Will be set to Some(...) when an asynchronous fetch operation is
            // kicked off:
            poll_task: None,
        });

        // Release our lock on the job and hand over to the fetch image method:
        std::mem::drop(job_lg);
        Self::fetch_image(this, job_id, 0 /* first iteration */).await;

        Ok(())
    }

    #[instrument(skip(this), err(Debug, level = Level::WARN))]
    async fn stop_job(
        this: &Arc<Self>,
        msg: connector::StopJobMessage,
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
                    error_kind: connector::JobErrorKind::AlreadyStopping,
                    description: format!("Job {:?} is already stopping.", msg.job_id),
                });
            }

            // Handle the case where the job might have already finished
            prev_state => {
                // Put back the previous state:
                *job_lg = prev_state;

                return Err(connector::JobError {
                    error_kind: connector::JobErrorKind::JobNotRunning,
                    description: format!("Job {:?} is not running.", msg.job_id),
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
                image_id: _,
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
                image_id: _,
                manifest: _,
            }) => {
                // Nothing to do, the only time we're seeing this state is in
                // between fetching image, and actually starting the job. Given
                // that we've acquired the lock between those two functions, we
                // don't need to deallocate, shut down or abort anything.
            }

            StopJobPrevState::Running(QemuSupervisorJobRunningState {
                start_job_req: _,
                control_socket,
                qemu_proc,
                qemu_monitor_task,
            }) => {
                // Attempt to kill the QEMU process
                {
                    let mut qemu_proc = qemu_proc.lock().await;
                    if let Err(e) = qemu_proc.kill().await {
                        event!(
                            Level::WARN,
                            ?e,
                            "Failed to kill QEMU process for job {job_id}"
                        );
                    }
                }

                // Wait for the monitoring task to complete
                if let Err(e) = qemu_monitor_task.await {
                    event!(Level::WARN, ?e, "QEMU monitor task for job {job_id} failed");
                }

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
    #[instrument(skip(self))]
    async fn ssh_keys(&self, tgt_job_id: Uuid) -> Option<Vec<String>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                QemuSupervisorJobState::Running(QemuSupervisorJobRunningState {
                    start_job_req,
                    ..
                }) => Some(start_job_req.ssh_keys.clone()),

                // Only respond to host / puppet requests when the job is marked
                // as "running":
                state => {
                    event!(
			Level::WARN,
                            "Received puppet SSH keys request for job {job_id} in invalid state {job_state}",
                        job_id = tgt_job_id,
                        job_state = state.state_name(),
                    );
                    None
                }
            },

            // Job not found:
            None => {
                event!(
                    Level::WARN,
                    "Received puppet SSH keys request for non-existant job {job_id}",
                    job_id = tgt_job_id,
                );
                None
            }
        }
    }

    #[instrument(skip(self))]
    async fn network_config(
        &self,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::NetworkConfig> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running, respond with its assigned hostname:
                QemuSupervisorJobState::Running(_) => {
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
                    event!(
			Level::WARN,
                        "Received puppet network config request for job {job_id} in invalid state {job_state}",
                        job_id = tgt_job_id,
                        job_state = state.state_name(),
                    );
                    None
                }
            },

            // Job not found:
            None => {
                event!(
                    Level::WARN,
                    "Received puppet network config request for non-existant job {job_id}",
                    job_id = tgt_job_id,
                );
                None
            }
        }
    }

    #[instrument(skip(self))]
    async fn parameters(
        &self,
        tgt_job_id: Uuid,
    ) -> Option<HashMap<String, treadmill_rs::api::supervisor_puppet::ParameterValue>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running:
                QemuSupervisorJobState::Running(QemuSupervisorJobRunningState {
                    start_job_req,
                    ..
                }) => Some(start_job_req.parameters.clone()),

                // Only respond to host / puppet requests when the job is marked
                // as "running":
                state => {
                    event!(
			Level::WARN,
                        "Received puppet parameters request for job {job_id} in invalid state {job_state}",
                        job_id = tgt_job_id,
                        job_state = state.state_name(),
                    );
                    None
                }
            },

            // Job not found:
            None => {
                event!(
                    Level::WARN,
                    "Received puppet parameters request for non-existant job {job_id}",
                    job_id = tgt_job_id,
                );
                None
            }
        }
    }

    #[instrument(skip(self))]
    async fn puppet_ready(&self, _puppet_event_id: u64, job_id: Uuid) {
        event!(Level::INFO, "Received puppet ready event");

        match self.jobs.lock().await.get(&job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running, forward this event to a `JobState`
                // change towards `Ready`:
                QemuSupervisorJobState::Running(_) => {
                    self.connector
                        .update_job_state(
                            job_id,
                            connector::JobState::Ready {
                                // TODO: populate connection info
                                connection_info: vec![],
                                status_message: None,
                            },
                        )
                        .await;
                }

                // Only respond to host / puppet requests when the job is marked
                // as "running":
                state => {
                    event!(
                        Level::WARN,
                        "Received puppet ready event in invalid state {job_state}",
                        job_state = state.state_name(),
                    );
                }
            },

            // Job not found:
            None => {
                event!(
                    Level::WARN,
                    "Received puppet parameters request for non-existant job",
                );
            }
        }
    }

    #[instrument(skip(self))]
    async fn puppet_shutdown(
        &self,
        _puppet_event_id: u64,
        _supervisor_event_id: Option<u64>,
        _job_id: Uuid,
    ) {
        event!(Level::INFO, "Received puppet shutdown event",);

        // We don't want to do any proper job-state transition here, as this
        // input is controlled by the puppet. It may simply claim to be
        // rebooting or shutting down, but not actually doing this. We want the
        // `JobState` transitions to be well-defined, and governed by the
        // supervisor, not the host.
        //
        // As an alternative, we should -- in the `Ready` state -- introduce a
        // new field that shows the reported state from the puppet, for instance
        // whether it claims to be rebooting or shutting down.
        //
        // The `Stopping` state is then only set for when the QEMU process is
        // stopped, or when a shutdown is invoked from within the supervisor.
    }

    #[instrument(skip(self))]
    async fn puppet_reboot(
        &self,
        _puppet_event_id: u64,
        _supervisor_event_id: Option<u64>,
        _job_id: Uuid,
    ) {
        event!(Level::INFO, "Received puppet reboot event",);

        // We don't want to do any proper job-state transition here, as this
        // input is controlled by the puppet. It may simply claim to be
        // rebooting or shutting down, but not actually doing this. We want the
        // `JobState` transitions to be well-defined, and governed by the
        // supervisor, not the host.
        //
        // As an alternative, we should -- in the `Ready` state -- introduce a
        // new field that shows the reported state from the puppet, for instance
        // whether it claims to be rebooting or shutting down.
        //
        // The `Stopping` state is then only set for when the QEMU process is
        // stopped, or when a shutdown is invoked from within the supervisor.
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    use treadmill_rs::connector::SupervisorConnector;

    tracing_subscriber::fmt().init();
    event!(Level::INFO, "Treadmill Qemu Supervisor, Hello World!");

    let args = QemuSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: QemuSupervisorConfig = toml::from_str(&config_str).unwrap();

    let image_store =
        image_store_client::ImageStoreClient::new(config.image_store.http_endpoint.clone())
            .await
            .unwrap()
            .into_local(&config.image_store.fs_endpoint)
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
        SupervisorCoordConnector::WsConnector => {
            let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                "Requested WsConnector, but `ws_connector` config not present."
            ))?;

            // Both the supervisor and connectors have references to each other,
            // so we break the cyclic dependency with an initially unoccupied
            // weak Arc reference:
            let mut connector_opt = None;

            let qemu_supervisor = {
                // Shadow, to avoid moving the variable:
                let connector_opt = &mut connector_opt;
                Arc::new_cyclic(move |weak_supervisor| {
                    let connector = Arc::new(tml_ws_connector::WsConnector::new(
                        config.base.supervisor_id,
                        ws_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    QemuSupervisor::new(connector, image_store, args, config)
                })
            };

            let connector = connector_opt.take().unwrap();

            connector.run().await;

            drop(qemu_supervisor);

            Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
}
