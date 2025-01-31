use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_recursion::async_recursion;
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::fs;
use tokio::sync::Mutex;
use tracing::{event, instrument, Level};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageSpecification, JobInitializingStage, RunningJobState,
};

use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::image;
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_tcp_control_socket_server::TcpControlSocket;

use treadmill_supervisor_lib::image_store_client;

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
pub struct NbdNetbootSupervisorArgs {
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
pub struct NbdNetbootConfig {
    /// QEMU NBD server binary.
    qemu_nbd_binary: PathBuf,

    /// `qemu-img` binary, to work with qcow2 files.
    qemu_img_binary: PathBuf,

    /// `tar` binary, to pack and unpack the boot TFTP archive.
    tar_binary: PathBuf,

    /// Directory to keep state:
    state_dir: PathBuf,

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
    nbd_server_listen_addr: std::net::SocketAddr,

    /// TFTP boot file system path.
    tftp_boot_dir: PathBuf,

    /// Start the netboot target.
    ///
    /// This script should ensure that the netboot target is powered on. If it
    /// is already powered on, it must power-cycle it.
    start_script: PathBuf,

    /// Stop the netboot target.
    ///
    /// This script should ensure that the netboot target is powered off. If it
    /// is already powered off, it must power-cycle it.
    stop_script: PathBuf,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NbdNetbootSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configurations for individual connector implementations. All are
    /// optional, and not all of them have to be supported:
    cli_connector: Option<treadmill_cli_connector::CliConnectorConfig>,

    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    image_store: image_store_client::LocalImageStoreConfig,

    nbd_netboot: NbdNetbootConfig,
}

#[derive(Debug)]
pub struct NbdNetbootSupervisorFetchingImageState {
    start_job_req: connector::StartJobMessage,
    image_id: image::manifest::ImageId,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
pub struct NbdNetbootSupervisorImageFetchedState {
    start_job_req: connector::StartJobMessage,
    image_id: image::manifest::ImageId,
    manifest: image::manifest::ImageManifest,
}

#[derive(Debug)]
pub struct NbdNetbootSupervisorJobRunningState {
    start_job_req: connector::StartJobMessage,
    job_workdir: PathBuf,

    /// Control socket handle:
    control_socket: TcpControlSocket<NbdNetbootSupervisor>,

    /// Sender to signal the monitoring task to stop the process.
    ///
    /// Only one message may be sent, after which is will turn into a `None`:
    shutdown_tx: Option<tokio::sync::oneshot::Sender<NbdNetbootSupervisorJobRunningState>>,
}

#[derive(Debug)]
pub enum NbdNetbootSupervisorJobState {
    /// State to indicate that the job is starting.
    ///
    /// We use this to reserve a spot in the [`NbdNetbootSupervisor`]'s `jobs` map,
    /// such that we can release the global HashMap lock afterwards.
    Starting,

    /// State to indicate that we're currently waiting on the image to
    /// be fetched. This state polls the image store with a fixed
    /// interval.
    FetchingImage(NbdNetbootSupervisorFetchingImageState),

    /// The job's image has been fully fetched, and we're allocating resources
    /// and starting it. It's not fully up and running yet.
    ImageFetched(NbdNetbootSupervisorImageFetchedState),

    /// State to indicate that the job is running.
    Running(NbdNetbootSupervisorJobRunningState),

    /// State to indicate that the job is currently shutting down.
    ///
    /// While the job is in this state, no job with the same ID must be started
    /// / resumed. We might still be cleaning up resources associated with this
    /// job.
    Stopping,
}

impl NbdNetbootSupervisorJobState {
    fn state_name(&self) -> &'static str {
        match self {
            NbdNetbootSupervisorJobState::Starting => "Starting",
            NbdNetbootSupervisorJobState::FetchingImage(_) => "FetchingImage",
            NbdNetbootSupervisorJobState::ImageFetched(_) => "ImageFetched",
            NbdNetbootSupervisorJobState::Running(_) => "Running",
            NbdNetbootSupervisorJobState::Stopping => "Stopping",
        }
    }
}

#[derive(Debug)]
pub struct NbdNetbootSupervisor {
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
    jobs: Mutex<HashMap<Uuid, Arc<Mutex<NbdNetbootSupervisorJobState>>>>,

    _args: NbdNetbootSupervisorArgs,
    config: NbdNetbootSupervisorConfig,
}

impl NbdNetbootSupervisor {
    pub fn new(
        connector: Arc<dyn connector::SupervisorConnector>,
        image_store_client: image_store_client::LocalImageStoreClient,
        args: NbdNetbootSupervisorArgs,
        config: NbdNetbootSupervisorConfig,
    ) -> Self {
        NbdNetbootSupervisor {
            connector,
            image_store_client,
            jobs: Mutex::new(HashMap::new()),
            _args: args,
            config,
        }
    }

    #[instrument(skip(self))]
    async fn remove_job(&self, job_id: Uuid) -> Option<Arc<Mutex<NbdNetbootSupervisorJobState>>> {
        event!(Level::DEBUG, "Removing job from jobs map");
        self.jobs.lock().await.remove(&job_id)
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
            NbdNetbootSupervisorJobState::Starting,
        ) {
            NbdNetbootSupervisorJobState::FetchingImage(state) => state,
            prev_state => {
                // Swap the old state back:
                *job_lg = prev_state;

                // Same as above, let's issue a debug message:
                event!(
                    Level::DEBUG,
                    "Job not in FetchingImage state (likely changed state across invocations of fetch_image): {job:?}. Returning.",
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
                        *job_lg = NbdNetbootSupervisorJobState::Stopping;

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
                *job_lg = NbdNetbootSupervisorJobState::ImageFetched(
                    NbdNetbootSupervisorImageFetchedState {
                        start_job_req: fetching_image_state.start_job_req,
                        image_id: fetching_image_state.image_id,
                        manifest,
                    },
                );

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
                        RunningJobState::Initializing {
                            stage: JobInitializingStage::FetchingImage,
                        },
                        msg,
                    )
                    .await;

                // Put the job into the `FetchingImage` state:
                event!(Level::DEBUG, "Transitioning job into FetchingImage state");
                *job_lg = NbdNetbootSupervisorJobState::FetchingImage(
                    NbdNetbootSupervisorFetchingImageState {
                        poll_task: Some(poll_task),
                        ..fetching_image_state
                    },
                );
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
                *job_lg = NbdNetbootSupervisorJobState::Stopping;
            }
        }
    }

    #[instrument(skip(self, manifest), err(Debug, level = Level::WARN))]
    async fn start_job_parse_manifest_check_images(
        &self,
        manifest: &image::manifest::ImageManifest,
    ) -> Result<(PathBuf, PathBuf, QemuImgMetadata)> {
        event!(
            Level::DEBUG,
            ?manifest,
            "Checking & extracting head-image layer from image manifest"
        );

        // Retrieve the "top-most" layer in the image manifest, from which we'll
        // create our "working" disk image overlay:
        let top_layer_label = manifest
            .attrs
            .get("org.tockos.treadmill.image.nbd_qcow2_layered_v0.head")
            .ok_or(anyhow!("Image does not have a head layer defined"))?;

        // Check that we have a boot archive defined:
        let boot_blob_label = format!("{}-boot", top_layer_label);
        let boot_blob_spec = manifest
            .blobs
            .get(&boot_blob_label)
            .ok_or_else(|| anyhow!("Image missing blob {}", boot_blob_label))?;

        // Get the path to the boot archive:
        let boot_blob_path = self
            .image_store_client
            .blob_path(&boot_blob_spec.sha256_digest)
            .await;

        let boot_blob_path_canon = tokio::fs::canonicalize(&boot_blob_path)
            .await
            .with_context(|| {
                format!("Failed to canonicalize boot blob path {:?}", boot_blob_path)
            })?;

        // Now step through each image layer, making sure that it has a root
        // disk and (if that has a predecessor defined) the current root blob
        // points to that predecessor. We don't want images to be able to
        // reference arbitrary paths in our filesystem as underlying layers.
        //
        // We use `qemu-img` to retrieve the (partial) metadata of an image.

        // Now, for every image in the chain, parse the metadata and make sure
        // that the image file:
        //
        // - Exists,
        // - Matches its specification in the manifest, and
        // - Only references its defined predecessor.
        let mut top_root_image = None;
        let mut current_root_image = format!("{}-root", top_layer_label);
        loop {
            // Acquire the manifest metadata for this image blob:
            let blob_spec = manifest
                .blobs
                .get(&current_root_image)
                .ok_or_else(|| anyhow!("Image missing blob {}", current_root_image))?;

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

            event!(Level::DEBUG, qemu_img_binary = ?self.config.nbd_netboot.qemu_img_binary, file = ?image_path_canon, "Retrieving qcow2 file metadata");
            let metadata_output =
                tokio::process::Command::new(&self.config.nbd_netboot.qemu_img_binary)
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
                    .with_context(|| {
                        format!("Failed to query image metadata for {:?}", image_path)
                    })?;

            let metadata: QemuImgMetadata =
                serde_json::from_slice(&metadata_output).with_context(|| {
                    format!(
                        "Failed to parse qemu-img info output for {:?}: {:?}",
                        image_path,
                        String::from_utf8_lossy(&metadata_output),
                    )
                })?;

            top_root_image.get_or_insert_with(|| {
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
                .get("org.tockos.treadmill.image.nbd_qcow2_layered_v0.blob-virtual-size")
                .ok_or_else(|| {
                    anyhow!(
                        "Image blob {} missing `blob-virtual-size` attribute",
                        current_root_image
                    )
                })
                .and_then(|size_bytes_str| {
                    u64::from_str_radix(size_bytes_str, 10).with_context(|| {
                        format!(
                            "Parsing blob {}'s `blob-virtual-size` attribute as u64",
                            current_root_image
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
                .get("org.tockos.treadmill.image.nbd_qcow2_layered_v0.lower");
            if let Some(blob_lower) = blob_lower_opt {
                // We do have a lower blob, look it up in the image store:
                let lower_blob_spec = manifest.blobs.get(blob_lower).ok_or_else(|| {
                    anyhow!(
                        "Image missing blob {}, referenced by blob {}",
                        blob_lower,
                        current_root_image,
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
                            f, current_root_image,
                        )
                    })?
                } else {
                    bail!(
                        "Image {} supposed to reference blob {}, but \
                         qemu-img references no backing file",
                        current_root_image,
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
                        current_root_image,
                        full_backing_filename_canon,
                    );
                }

                // All checks passed, continue with the next layer:
                current_root_image = blob_lower.to_string();
            } else {
                // Image is not supported to have any lower / backing layer:
                if metadata.backing_filename.is_some() || metadata.full_backing_filename.is_some() {
                    bail!(
                        "Image blob {} (file {:?}) does not have a \
                         lower-blob specified but references a backing file",
                        current_root_image,
                        image_path_canon,
                    );
                }

                // We've reached the end of the image chain without error, break
                // out of the loop.
                break;
            }
        }

        // If we reach this point, we must have a path to the top root layer of
        // the image. Return it and the boot archive:
        let (top_root_image_path, top_root_image_metadata) = top_root_image.unwrap();
        Ok((
            boot_blob_path_canon,
            top_root_image_path,
            top_root_image_metadata,
        ))
    }

    #[instrument(skip(self, start_job_req, boot_archive_path), err(Debug, level = Level::WARN))]
    async fn start_job_unpack_boot_archive(
        &self,
        start_job_req: &connector::StartJobMessage,
        boot_archive_path: &Path,
    ) -> Result<(), connector::JobError> {
        let tftp_boot_dir = &self.config.nbd_netboot.tftp_boot_dir;

        event!(
            Level::DEBUG,
            ?tftp_boot_dir,
            "Ensuring that TFTP boot directory exists"
        );
        fs::create_dir_all(tftp_boot_dir)
            .await
            .map_err(|io_err| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Unable to create (parents of) TFTP boot directory at {:?}: {:?}",
                    tftp_boot_dir, io_err
                ),
            })?;

        // We need to empty this directory without deleting the directory itself
        // (which may reside on a special mountpoint / volume or have directory
        // attributes that we should preserve):
        event!(Level::DEBUG, ?tftp_boot_dir, "Emptying TFTP boot directory");

        // TODO: we wrap this into a function to map the IOError onto a JobError
        // below. Once try-blocks are stabilized, this can be turned into a
        // simple block: https://doc.rust-lang.org/unstable-book/language-features/try-blocks.html
        async fn empty_tftp_boot_dir(tftp_boot_dir: &Path) -> tokio::io::Result<()> {
            let mut entries = fs::read_dir(tftp_boot_dir).await?;

            // New errors may be encountered while iterating over the file
            // system entries themselves:
            while let Some(entry) = entries.next_entry().await? {
                // Delete the entry. We need to distinguish between directories
                // and other files:
                if entry.file_type().await?.is_dir() {
                    fs::remove_dir_all(entry.path()).await?;
                } else {
                    fs::remove_file(entry.path()).await?;
                }
            }

            Ok(())
        }
        empty_tftp_boot_dir(tftp_boot_dir)
            .await
            .map_err(|io_err| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Unable to delete files of TFTP boot directory at {:?}: {:?}",
                    tftp_boot_dir, io_err
                ),
            })?;

        // We need to empty this directory without deleting the directory itself
        // (which may reside on a special mountpoint / volume or have directory
        // attributes that we should preserve):
        event!(
            Level::DEBUG,
            ?tftp_boot_dir,
            ?boot_archive_path,
            "Unpacking boot TFTP archive"
        );

        tokio::process::Command::new(&self.config.nbd_netboot.tar_binary)
	    // Extract:
            .arg("-x")
            // Target directory:
            .arg("-C")
	    .arg(&tftp_boot_dir)
	    // Boot archive:
            .arg("-f")
            .arg(&boot_archive_path)
            .output()
            .await
            .map_err(|e| anyhow::Error::from(e))
            .and_then(|output| {
                // Ideally we'd want to use the nightly `exit_ok()` here:
                if !output.status.success() {
                    bail!(
                        "Extracting boot archive failed with exit-code {:?}, stdout: {:?}, stderr: {:?}",
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
                    "Failed to extract boot archive {:?} from image to {:?} for job {:?}: {:?}",
                    boot_archive_path, tftp_boot_dir, start_job_req.job_id, e,
                ),
            })?;

        Ok(())
    }

    #[instrument(skip(self, start_job_req, top_root_image_qemu_info), err(Debug, level = Level::WARN))]
    async fn start_job_parse_manifest_allocate_root_disk(
        &self,
        start_job_req: &connector::StartJobMessage,
        top_root_image_path: &Path,
        top_root_image_qemu_info: &QemuImgMetadata,
    ) -> Result<(PathBuf, PathBuf), connector::JobError> {
        let jobs_dir = self.config.nbd_netboot.state_dir.join("jobs");
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

        // Canonicalize job dir, so that we are independent of the
        // process' working directories we'll be passing it into:
        let job_dir = fs::canonicalize(job_dir)
            .await
            .expect("Job dir does not exist despite just having been created.");

        // Create a new thin-provisioned disk image with the specified size. The
        // virtual machine image can then be expanded.
        //
        // We want to make sure that the new CoW disk image layer is not smaller
        // than the one specified in the manifest for the image.
        if top_root_image_qemu_info.virtual_size > self.config.nbd_netboot.working_disk_max_bytes {
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
        event!(
            Level::DEBUG,
            backing_image = ?top_root_image_path,
            ?disk_image_file,
            virtual_size_bytes = self.config.nbd_netboot.working_disk_max_bytes,
            "Creating job disk image"
        );
        tokio::process::Command::new(&self.config.nbd_netboot.qemu_img_binary)
            .current_dir(&job_dir)
            .arg("create")
            // Image format:
            .arg("-f")
            .arg("qcow2")
            // Backing file format:
            .arg("-F")
            .arg("qcow2")
            // Backing file:
            .arg("-b")
            .arg(&top_root_image_path)
            // New image file:
            .arg(&disk_image_file)
            // New image size (in bytes, without suffix):
            .arg(&self.config.nbd_netboot.working_disk_max_bytes.to_string())
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
        let NbdNetbootSupervisorImageFetchedState {
            start_job_req,
            image_id,
            manifest,
        } = match std::mem::replace(&mut *job_lg, NbdNetbootSupervisorJobState::Starting) {
            NbdNetbootSupervisorJobState::ImageFetched(state) => state,
            prev_state => {
                // Swap the old state back:
                *job_lg = prev_state;

                // Same as above, let's issue a debug message:
                event!(
                    Level::DEBUG,
                    "Job not in ImageFetched state (likely changed state before start_job_cont could acquire a lock): {job:?}",
                );

                return;
            }
        };

        // Inform the connector that we're now preparing for start:
        this.connector
            .update_job_state(
                start_job_req.job_id,
                RunningJobState::Initializing {
                    stage: JobInitializingStage::Allocating,
                },
                Some(String::new()),
            )
            .await;

        // Parse the manifest and sanity-check all image layers. We do this in
        // an async block such that we can use the ``?`` operator:
        let (boot_archive_path, top_root_image_path, top_root_image_qemu_info) =
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
                    *job_lg = NbdNetbootSupervisorJobState::Stopping;

                    return;
                }
            };

        // Unpack the boot archive:
        if let Err(job_error) = this
            .start_job_unpack_boot_archive(&start_job_req, &boot_archive_path)
            .await
        {
            // Failed to unpack the boot archive, report the error:
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
            *job_lg = NbdNetbootSupervisorJobState::Stopping;

            return;
        }

        // Allocate the job's disk:
        let (job_workdir, job_disk_image_path) = match this
            .start_job_parse_manifest_allocate_root_disk(
                &start_job_req,
                &top_root_image_path,
                &top_root_image_qemu_info,
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
                *job_lg = NbdNetbootSupervisorJobState::Stopping;

                return;
            }
        };

        // Start a TCP control socket on the specified listen addr:
        let control_socket = TcpControlSocket::new(
            this.config.base.supervisor_id,
            start_job_req.job_id,
            this.config.nbd_netboot.tcp_control_socket_listen_addr,
            this.clone(),
        )
        .await
        .unwrap();

        event!(Level::INFO, qemu_nbd_binary = ?this.config.nbd_netboot.qemu_nbd_binary, /* ?qemu_args, */ "Launching qemu-nbd server");
        let qemu_proc = tokio::process::Command::new(&this.config.nbd_netboot.qemu_nbd_binary)
            .current_dir(&job_workdir)
            .arg("--aio=io_uring")
            .arg("--discard=unmap")
            .arg("--detect-zeroes=unmap")
            .arg("--format=qcow2")
            .arg("--export-name=root")
            .arg("--persistent")
            .arg("--shared=0")
            .arg("--bind")
            .arg(
                this.config
                    .nbd_netboot
                    .nbd_server_listen_addr
                    .ip()
                    .to_string(),
            )
            .arg("--port")
            .arg(
                this.config
                    .nbd_netboot
                    .nbd_server_listen_addr
                    .port()
                    .to_string(),
            )
            .arg(&job_disk_image_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();

        event!(Level::INFO, start_script = ?this.config.nbd_netboot.start_script, "Running job start script");
        let start_script_res = tokio::process::Command::new(&this.config.nbd_netboot.start_script)
	    .current_dir(&job_workdir)
            .output()
            .await
            .map_err(|e| anyhow::Error::from(e))
            .and_then(|output| {
                // Ideally we'd want to use the nightly `exit_ok()` here:
                if !output.status.success() {
                    bail!(
                        "Extracting boot archive failed with exit-code {:?}, stdout: {:?}, stderr: {:?}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }

                // Don't care about stdout or stderr in the success case:
                Ok(())
            });

        if let Err(start_script_err) = start_script_res {
            this.connector
                .report_job_error(
                    start_job_req.job_id,
                    connector::JobError {
                        error_kind: connector::JobErrorKind::InternalError,
                        description: format!(
                            "Failed to run start script {:?} for job {:?}: {:?}",
                            this.config.nbd_netboot.start_script,
                            start_job_req.job_id,
                            start_script_err,
                        ),
                    },
                )
                .await;

            // TODO: deallocate resources!
            this.remove_job(job_id).await;

            // Prevent other tasks from further advancing this job's state:
            *job_lg = NbdNetbootSupervisorJobState::Stopping;

            return;
        }

        // Job has been started, let the coordinator know:
        this.connector
            .update_job_state(
                start_job_req.job_id,
                RunningJobState::Initializing {
                    // Booting, but puppet has not yet reported "ready":
                    stage: JobInitializingStage::Booting,
                },
                None,
            )
            .await;

        // Create a oneshot channel to signal shutdown
        let (shutdown_tx, shutdown_rx) =
            tokio::sync::oneshot::channel::<NbdNetbootSupervisorJobRunningState>();

        // Clone necessary variables for the monitoring task
        let supervisor_clone = Arc::clone(this);
        let job_id_clone = job_id;
        let job_clone = job.clone();

        // Spawn the monitoring task
        tokio::spawn(async move {
            Self::process_monitor(
                &supervisor_clone,
                job_id_clone,
                job_clone,
                qemu_proc,
                shutdown_rx,
            )
            .await
        });

        // Update the job state
        *job_lg = NbdNetbootSupervisorJobState::Running(NbdNetbootSupervisorJobRunningState {
            start_job_req,
            control_socket,
            shutdown_tx: Some(shutdown_tx),
            job_workdir,
        });
    }

    async fn process_monitor(
        this: &Arc<Self>,
        job_id: Uuid,
        job: Arc<Mutex<NbdNetbootSupervisorJobState>>,
        mut qemu_proc: tokio::process::Child,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<NbdNetbootSupervisorJobRunningState>,
    ) {
        let (terminate_message, job_running_state): (String, NbdNetbootSupervisorJobRunningState) = loop {
            tokio::select! {
                // When the qemu-nbd process has exited but, at the time of
                // receiving this event the job was no longer in the stopping
                // state, then there must be a message in the shutdown_rx
                // channel. The `stop_job` function will be waiting on this task
                // to exit and move the `job_running_state` back to it.
                //
                // However, if we were to simply always poll for the qemu-nbd
                // exit, we'd be stuck in an infinite loop in this case. Thus,
                // it's important that we check the `shutdown_rx` channel first:
                // if it contains a value, extract it and forward it to the
                // `finish_running_job_shutdown` function. If not, check whether
                // the qemu-nbd process exited on its own.
                //
                // We achieve this polling order by making this select `biased`:
                biased;

                job_running_state_res = &mut shutdown_rx => {
                    let job_running_state = job_running_state_res
                        .expect("Error receving from oneshot shutdown_rx channel");

                    // Received shutdown signal from the stop_job handler
                    // itself, which means that we're asked to kill the qemu-nbd
                    // process:
                    if let Err(e) = qemu_proc.kill().await {
                        // We were unable to terminate the QEMU process:
                        let message = format!("Failed to wait on qemu-nbd process: {:?}", e);

                        this.connector.report_job_error(
                            job_id,
                            connector::JobError {
                                error_kind: connector::JobErrorKind::InternalError,
                                description: message.clone(),
                            },
                        ).await;

                        break (message, job_running_state);
                    } else {
                        // Notify that the job was stopped
                        break ("qemu-nbd process was stopped successfully.".to_string(), job_running_state);
                    }
                }

                exit_status = qemu_proc.wait() => {
                    event!(
                        Level::DEBUG,
                        ?job_id,
                        "Process monitor task has noticed qemu-nbd process exit"
                    );

                    // qemu-nbd process exited
                    let terminate_message = match exit_status {
                        Ok(status) => {
                            if status.success() {
                                // Notify success
                                "qemu-nbd process exited successfully.".to_string()
                            } else {
                                // Notify failure
                                let message = format!("qemu-nbd process had an internal error with status: {:?}", status);

                                this.connector.report_job_error(
                                    job_id,
                                    connector::JobError {
                                        error_kind: connector::JobErrorKind::InternalError,
                                        description: message.clone(),
                                    },
                                ).await;

                                // Also terminate with an appropriate message:
                                message
                            }
                        }
                        Err(e) => {
                            // Failed to wait on qemu-nbd process
                            let message = format!("Failed to wait on qemu-nbd process: {:?}", e);

                            this.connector.report_job_error(
                                job_id,
                                connector::JobError {
                                    error_kind: connector::JobErrorKind::InternalError,
                                    description: message.clone(),
                                },
                            ).await;

                            // Also terminate with an appropriate message:
                            message
                        }
                    };
                    event!(
                        Level::DEBUG,
                        ?job_id,
                        "qemu-nbd terminate message: {:?}",
                        terminate_message
                    );

                    // Get a hold of the job lock and extract the job running
                    // state. If it no longer contains a running state, then the
                    // qemu-nbd process exit raced with a call to `stop_job`, which
                    // will place the job in the stopping state and send a
                    // message with the `job_running_state` on the shutdown
                    // oneshot channel. Thus, in this case, just continue into
                    // the next loop iteration, which should receive this
                    // message from `shutdown_rx`:
                    event!(
                        Level::DEBUG,
                        ?job_id,
                        "Process monitor task acquiring lock and extracting job state"
                    );
                    let mut job_lg = job.lock().await;

                    match std::mem::replace(&mut *job_lg, NbdNetbootSupervisorJobState::Stopping) {
                        NbdNetbootSupervisorJobState::Running(s) => {
                            break (terminate_message, s);
                        }

                        prev_state => {
                            // Put back the previous state:
                            *job_lg = prev_state;
                            event!(
                                Level::INFO,
                                ?job_id,
                                "Received qemu-nbd process exit, but job is no \
                                 longer in running state. This can occur when a \
                                 `stop_job` races with the running state. \
                                 Giving up and waiting on a shutdown signal \
                                 instead."
                            );

                            // Continue into the next loop iteration:
                        }
                    }

                    // Lock is implicitly released here:
                }
            }
        };

        // Finish the shutdown process:
        this.finish_running_job_shutdown(job_id, job, terminate_message, job_running_state)
            .await
    }

    async fn stop_job_internal(&self, job_id: Uuid) -> Result<(), connector::JobError> {
        // Get a reference to this job by an emphemeral lock on `jobs` HashMap:
        let job: Arc<Mutex<NbdNetbootSupervisorJobState>> = {
            self.jobs
                .lock()
                .await
                .get(&job_id)
                .cloned()
                .ok_or(connector::JobError {
                    error_kind: connector::JobErrorKind::JobNotFound,
                    description: format!("Job {:?} not found, cannot stop.", job_id),
                })?
        };

        let mut job_lg = job.lock().await;

        enum StopJobPrevState {
            FetchingImage(NbdNetbootSupervisorFetchingImageState),
            ImageFetched(NbdNetbootSupervisorImageFetchedState),
            Running(NbdNetbootSupervisorJobRunningState),
        }

        // Make sure the job is in a state in which we can stop it. If so, place
        // it into the Stopping state.
        let prev_job_state =
            match std::mem::replace(&mut *job_lg, NbdNetbootSupervisorJobState::Stopping) {
                // Stoppable states:
                NbdNetbootSupervisorJobState::FetchingImage(s) => {
                    StopJobPrevState::FetchingImage(s)
                }
                NbdNetbootSupervisorJobState::ImageFetched(s) => StopJobPrevState::ImageFetched(s),
                NbdNetbootSupervisorJobState::Running(s) => StopJobPrevState::Running(s),

                prev_state @ NbdNetbootSupervisorJobState::Starting => {
                    // Put back the previous state:
                    *job_lg = prev_state;

                    // We must never be able to acquire a lock over a job in
                    // this state. The job will atomically transition from
                    // `Starting` to some other state in the implementation of
                    // `start_job`. This state is just a placeholder in the
                    // global jobs map, such that no other job with the same ID
                    // can be started:
                    unreachable!("Job must not be in `Starting state!");
                }

                prev_state @ NbdNetbootSupervisorJobState::Stopping => {
                    // Put back the previous state:
                    *job_lg = prev_state;

                    return Err(connector::JobError {
                        error_kind: connector::JobErrorKind::AlreadyStopping,
                        description: format!("Job {:?} is already stopping.", job_id),
                    });
                }
            };

        // Job is stopping, let the coordinator know:
        self.connector
            .update_job_state(
                job_id,
                RunningJobState::Terminating,
                Some("Stop job requested".to_string()),
            )
            .await;

        // Perform actions depending on the previous job state:
        let terminate_message = match prev_job_state {
            StopJobPrevState::FetchingImage(NbdNetbootSupervisorFetchingImageState {
                start_job_req: _,
                image_id: _,
                poll_task,
            }) => {
                // Make sure that we don't have another poll task
                // scheduled. This is potentially racy, where this abort() call
                // can come right after an invocation of `Self::fetch_image` has
                // been scheduled. However, that function will not do anything,
                // given the job state is now `Stopping` instead of
                // `FetchingImage``:
                if let Some(handle) = poll_task {
                    handle.abort();
                }

                "Job stopped during `FetchingImage` stage.".to_string()
            }

            StopJobPrevState::ImageFetched(NbdNetbootSupervisorImageFetchedState {
                start_job_req: _,
                image_id: _,
                manifest: _,
            }) => {
                // Nothing to do, the only time we're seeing this state is in
                // between fetching image, and actually starting the job. Given
                // that we've acquired the lock between those two functions, we
                // don't need to deallocate, shut down or abort anything.
                "Job stopped during `ImageFetched` stage.".to_string()
            }

            StopJobPrevState::Running(mut running_state) => {
                let shutdown_tx = running_state.shutdown_tx.take().expect(
                    "`shutdown_tx` must not be None, only one such message \
                         may be sent while the job is in the running state",
                );

                // Send the shutdown signal
                let _ = shutdown_tx.send(running_state);

                // The `finish_running_job_shutdown` takes care of the remaining
                // shutdown procedure, including removing the job. We return here:
                return Ok(());
            }
        };

        // Job has been stopped, let the coordinator know:
        self.connector
            .update_job_state(job_id, RunningJobState::Terminated, Some(terminate_message))
            .await;

        // Finally, remove the job from the jobs HashMap. Eventually, all other
        // `Arc` references (including the one we hold) will get dropped.
        assert!(self.remove_job(job_id).await.is_some());

        Ok(())
    }

    async fn finish_running_job_shutdown(
        &self,
        job_id: Uuid,
        _job: Arc<Mutex<NbdNetbootSupervisorJobState>>,
        terminate_message: String,
        job_running_state: NbdNetbootSupervisorJobRunningState,
    ) {
        // When we reach this function, the qemu-nbd process has already
        // exited. Perform the remaining deallocation and shutdown procedures:

        let NbdNetbootSupervisorJobRunningState {
            control_socket,
            shutdown_tx: _,
            start_job_req: _,
            job_workdir,
        } = job_running_state;

        // Shut down the control socket server:
        control_socket.shutdown().await.unwrap();

        // Turn off the netboot target:
        event!(Level::INFO, stop_script = ?self.config.nbd_netboot.stop_script, "Running job stop script");
        let stop_script_res = tokio::process::Command::new(&self.config.nbd_netboot.stop_script)
	    .current_dir(&job_workdir)
            .output()
            .await
            .map_err(|e| anyhow::Error::from(e))
            .and_then(|output| {
                // Ideally we'd want to use the nightly `exit_ok()` here:
                if !output.status.success() {
                    bail!(
                        "Extracting boot archive failed with exit-code {:?}, stdout: {:?}, stderr: {:?}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }

                // Don't care about stdout or stderr in the success case:
                Ok(())
            });

        if let Err(stop_script_err) = stop_script_res {
            self.connector
                .report_job_error(
                    job_id,
                    connector::JobError {
                        error_kind: connector::JobErrorKind::InternalError,
                        description: format!(
                            "Failed to run stop script {:?} for job {:?}: {:?}",
                            self.config.nbd_netboot.stop_script, job_id, stop_script_err,
                        ),
                    },
                )
                .await;
        }

        // Job has been stopped, let the coordinator know:
        self.connector
            .update_job_state(job_id, RunningJobState::Terminated, Some(terminate_message))
            .await;

        // Finally, remove the job from the jobs HashMap. Eventually, all other
        // `Arc` references (including the one we hold) will get dropped.
        assert!(self.remove_job(job_id).await.is_some());
    }
}

#[async_trait]
impl connector::Supervisor for NbdNetbootSupervisor {
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

        // Don't start more jobs than we're allowed to. Currently, the Netboot
        // NBD supervisor only supports one job at a time (otherwise we'd need
        // to reason about IP address assignment from a pool, customizable
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
        let job = Arc::new(Mutex::new(NbdNetbootSupervisorJobState::Starting));

        // Acquire a lock on the job. No one else has a reference yet, so this
        // should succeed immediately:
        let mut job_lg = job.lock().await;

        // Insert a clone of the Arc into the HashMap:
        jobs_lg.insert(start_job_req.job_id, job.clone());

        // Release the global lock here:
        std::mem::drop(jobs_lg);
        //
        // ========== GLOBAL `jobs` HASHMAP LOCK RELEASED ======================

        // The job was inserted into the jobs HashMap and initialized as
        // `Starting`, let the coordinator know:
        this.connector
            .update_job_state(
                start_job_req.job_id,
                RunningJobState::Initializing {
                    // Generic starting stage. We don't fetch, allocate or provision any
                    // resources right now, so report a generic state instead:
                    stage: JobInitializingStage::Starting,
                },
                None,
            )
            .await;

        // Fetch the requested image.
        //
        // We avoid locking the job's state for long periods of time, e.g., to
        // allow cancelling it before the image has been fully fetched.  Thus,
        // we poll the image supervisor repeatedly, but don't hold onto the
        // job's lock in between these polling operations.

        let image_id = match start_job_req.image_spec {
            ImageSpecification::Image { image_id } => image_id,
            unsupported_init_spec => {
                unimplemented!("Unsupported init spec: {:?}", unsupported_init_spec)
            }
        };

        // Put the job into the `FetchingImage` state:
        let job_id = start_job_req.job_id; // Copy required below
        *job_lg =
            NbdNetbootSupervisorJobState::FetchingImage(NbdNetbootSupervisorFetchingImageState {
                start_job_req,
                image_id,
                // Will be set to Some(...) when an asynchronous fetch operation is
                // kicked off:
                poll_task: None,
            });

        // Release our lock on the job and hand over to the fetch image method:
        std::mem::drop(job_lg);

        Self::fetch_image(this, job_id, 0).await;

        Ok(())
    }

    #[instrument(skip(this), err(Debug, level = Level::WARN))]
    async fn stop_job(
        this: &Arc<Self>,
        msg: connector::StopJobMessage,
    ) -> Result<(), connector::JobError> {
        this.stop_job_internal(msg.job_id).await
    }
}

#[async_trait]
impl control_socket::Supervisor for NbdNetbootSupervisor {
    #[instrument(skip(self))]
    async fn ssh_keys(&self, _host_id: Uuid, tgt_job_id: Uuid) -> Option<Vec<String>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                NbdNetbootSupervisorJobState::Running(NbdNetbootSupervisorJobRunningState {
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
                    "Received puppet SSH keys request for non-existent job {job_id}",
                    job_id = tgt_job_id,
                );
                None
            }
        }
    }

    #[instrument(skip(self))]
    async fn network_config(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::NetworkConfig> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running, respond with its assigned hostname:
                NbdNetbootSupervisorJobState::Running(_) => {
                    let hostname = format!("job-{}", format!("{}", tgt_job_id).split_at(10).0);
                    Some(treadmill_rs::api::supervisor_puppet::NetworkConfig {
                        hostname,
                        // NbdNetbootSupervisor, don't supply a network interface to configure:
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
                    "Received puppet network config request for non-existent job {job_id}",
                    job_id = tgt_job_id,
                );
                None
            }
        }
    }

    #[instrument(skip(self))]
    async fn parameters(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<HashMap<String, treadmill_rs::api::supervisor_puppet::ParameterValue>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running:
                NbdNetbootSupervisorJobState::Running(NbdNetbootSupervisorJobRunningState {
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
                    "Received puppet parameters request for non-existent job {job_id}",
                    job_id = tgt_job_id,
                );
                None
            }
        }
    }

    #[instrument(skip(self))]
    async fn puppet_ready(&self, _puppet_event_id: u64, _host_id: Uuid, job_id: Uuid) {
        event!(Level::INFO, "Received puppet ready event");

        match self.jobs.lock().await.get(&job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running, forward this event to a `JobState`
                // change towards `Ready`:
                NbdNetbootSupervisorJobState::Running(_) => {
                    self.connector
                        .update_job_state(job_id, RunningJobState::Ready, None)
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
                    "Received puppet ready event for non-existent job",
                );
            }
        }
    }

    #[instrument(skip(self))]
    async fn puppet_shutdown(
        &self,
        _puppet_event_id: u64,
        _supervisor_event_id: Option<u64>,
        _host_id: Uuid,
        _job_id: Uuid,
    ) {
        event!(Level::INFO, "Received puppet shutdown event");

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
        // The `Stopping` state is then only set for when the qemu-nbd process
        // is stopped, or when a shutdown is invoked from within the supervisor.
    }

    #[instrument(skip(self))]
    async fn puppet_reboot(
        &self,
        _puppet_event_id: u64,
        _supervisor_event_id: Option<u64>,
        _host_id: Uuid,
        _job_id: Uuid,
    ) {
        event!(Level::INFO, "Received puppet reboot event");

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
        // The `Stopping` state is then only set for when the qemu-nbd process
        // is stopped, or when a shutdown is invoked from within the supervisor.
    }

    #[instrument(skip(self))]
    async fn terminate_job(
        &self,
        _puppet_event_id: u64,
        _supervisor_event_id: Option<u64>,
        _host_id: Uuid,
        job_id: Uuid,
    ) {
        event!(
            Level::INFO,
            ?job_id,
            "Received puppet event to terminate job",
        );

        if let Err(e) = self.stop_job_internal(job_id).await {
            event!(Level::WARN, "Failed to stop job: {:?}", e);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    use treadmill_rs::connector::SupervisorConnector;

    tracing_subscriber::fmt::init();
    event!(Level::INFO, "Treadmill NbdNetboot Supervisor, Hello World!");

    let args = NbdNetbootSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: NbdNetbootSupervisorConfig = toml::from_str(&config_str).unwrap();

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
                    let connector = Arc::new(treadmill_cli_connector::CliConnector::new(
                        config.base.supervisor_id,
                        cli_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    NbdNetbootSupervisor::new(connector, image_store, args, config)
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

            let _qemu_supervisor = {
                // Shadow, to avoid moving the variable:
                let connector_opt = &mut connector_opt;
                Arc::new_cyclic(move |weak_supervisor| {
                    let connector = Arc::new(treadmill_ws_connector::WsConnector::new(
                        config.base.supervisor_id,
                        ws_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    NbdNetbootSupervisor::new(connector, image_store, args, config)
                })
            };

            let connector = connector_opt.take().unwrap();

            loop {
                connector.run().await;
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
            }

            // when connector.run() can error replace with this code
            // loop {
            //     if let Err(e) = connector.run().await {
            //         eprintln!("Connection error: {:?}", e);
            //         tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            //     } else {
            //         // Connection ended gracefully
            //         break;
            //     }
            // }

            // Must drop qemu_supervisor reference _after_ connector.run(), as
            // that'll upgrade its Weak into an Arc. Otherwise we're dropping
            // the only reference to it:
            // std::mem::drop(qemu_supervisor);

            // Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
}
