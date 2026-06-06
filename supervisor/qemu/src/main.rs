use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{Level, event, info, instrument, warn};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageSpecification, JobInitializingStage, RunningJobState,
};

use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::image::Digest;
use treadmill_rs::image::blockdev::BackingChain;
use treadmill_rs::image::parse::{self, ImageLayer, TreadmillImage};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_tcp_control_socket_server::TcpControlSocket;

use treadmill_supervisor_lib::launcher::{self, ProcessLauncher, WorkloadProcess};
use treadmill_supervisor_lib::oci_store::{ImageStore, Location, OciStore, OciStoreConfig};

#[derive(Parser, Debug, Clone)]
pub struct QemuSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SSHPreferredIPVersion {
    #[default]
    Unspecified,
    V4,
    V6,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuConfig {
    /// Main QEMU binary to execute for a job.
    qemu_binary: PathBuf,

    /// `qemu-img` binary, to work with qcow2 files.
    qemu_img_binary: PathBuf,

    /// Directory to keep state:
    state_dir: PathBuf,

    /// Directory to keep state:
    /// List of arguments to pass to the QEMU binary.
    ///
    /// These arguments support template strings using the
    /// [`strfmt`](https://docs.rs/strfmt/latest/strfmt/) crate.q
    ///
    /// The available template strings are:
    ///
    /// - `job_id`: UUID as a hyphenated string
    ///
    /// - `job_workdir`: per-job state directory
    ///
    /// - `disk_node`: `node-name` of the writable top of the runtime backing
    ///   chain ([`BackingChain::TOP_NODE`]). The supervisor prepends the
    ///   `-blockdev` nodes assembling the chain to the invocation, so the
    ///   configured args should attach the disk device by referencing this
    ///   node, e.g. `-device virtio-blk-device,drive={disk_node}`.
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
    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    /// Local OCI store (per-server Zot daemon) the supervisor pulls images from
    /// and reads blob files out of directly.
    oci_store: OciStoreConfig,

    qemu: QemuConfig,
}

#[derive(Debug)]
pub struct QemuSupervisorFetchingImageState {
    start_job_req: connector::StartJobMessage,
    /// OCI manifest digest identifying the image (content-addressed).
    manifest_digest: Digest,
    /// Ordered, failover list of local-store locations serving the digest.
    locations: Vec<Location>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
pub struct QemuSupervisorImageFetchedState {
    start_job_req: connector::StartJobMessage,
    /// Repository the image was made present under in the local store.
    repository: String,
    /// The parsed Treadmill view of the OCI manifest (backing-chain layers).
    image: TreadmillImage,
}

#[derive(Debug)]
pub struct QemuSupervisorJobRunningState {
    start_job_req: connector::StartJobMessage,

    /// Control socket handle:
    control_socket: TcpControlSocket<QemuSupervisor>,

    /// Sender to signal the monitoring task to stop the process.
    ///
    /// Only one message may be sent, after which is will turn into a `None`:
    shutdown_tx: Option<tokio::sync::oneshot::Sender<QemuSupervisorJobRunningState>>,
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

    /// The job's image has been fully fetched, and we're allocating resources
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

    /// Read-only client of the local OCI store daemon (per-server Zot). We ask
    /// it to make a digest present, then open its on-disk blob files directly
    /// to assemble the backing chain. Injectable so the job state machine can
    /// be driven by tests with a stub store.
    image_store: Arc<dyn ImageStore>,

    /// Seam for the `qemu-img`/`qemu` subprocess operations, injectable so the
    /// job state machine can be driven by tests without spawning real binaries.
    launcher: Arc<dyn ProcessLauncher>,

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
        image_store: Arc<dyn ImageStore>,
        launcher: Arc<dyn ProcessLauncher>,
        args: QemuSupervisorArgs,
        config: QemuSupervisorConfig,
    ) -> Self {
        QemuSupervisor {
            connector,
            image_store,
            launcher,
            jobs: Mutex::new(HashMap::new()),
            _args: args,
            config,
        }
    }

    #[instrument(skip(self))]
    async fn remove_job(&self, job_id: Uuid) -> Option<Arc<Mutex<QemuSupervisorJobState>>> {
        event!(Level::DEBUG, "Removing job from jobs map");
        self.jobs.lock().await.remove(&job_id)
    }

    #[instrument(skip(this))]
    async fn fetch_image(this: &Arc<Self>, job_id: Uuid) {
        event!(Level::DEBUG, "Entering fetch_image");

        // This function is called once directly from `start_job`. If the job is
        // in any state other than `FetchingImage` (e.g. it was stopped in the
        // meantime), we just exit without doing anything:
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
                    "Job not in FetchingImage state (likely changed state across invocations of fetch_image): {job:?}. Returning.",
                );

                return;
            }
        };

        // Ask the local OCI store to make the manifest digest present — a
        // pull-through from one of the dispatched locations, or a cache hit —
        // then read+parse its manifest into the Treadmill backing-chain view.
        // `ensure_present` is a single operation (no in-progress polling), so on
        // success we transition straight to `ImageFetched` and continue.
        //
        // Don't post a `FetchingImage` status here: a cache hit is the common
        // case and reports straight through to `Allocating` (in `start_job_cont`).
        event!(
            Level::TRACE,
            manifest_digest = %fetching_image_state.manifest_digest,
            locations = ?fetching_image_state.locations,
            "Ensuring image present in the local OCI store",
        );

        // Helper: report a job error, drop the job, and wedge it into
        // `Stopping`. No resources are allocated yet at this point.
        macro_rules! fail {
            ($kind:expr, $desc:expr) => {{
                this.connector
                    .report_job_error(
                        fetching_image_state.start_job_req.job_id,
                        connector::JobError {
                            error_kind: $kind,
                            description: $desc,
                        },
                    )
                    .await;
                this.remove_job(job_id).await;
                *job_lg = QemuSupervisorJobState::Stopping;
                return;
            }};
        }

        let repository = match this
            .image_store
            .ensure_present(
                &fetching_image_state.manifest_digest,
                &fetching_image_state.locations,
            )
            .await
        {
            Ok(repository) => repository,
            Err(e) => {
                event!(Level::WARN, fetch_image_error = ?e, "Failed to fetch image");
                fail!(
                    connector::JobErrorKind::InternalError,
                    format!(
                        "Failed to fetch image {}: {e:#}",
                        fetching_image_state.manifest_digest,
                    )
                );
            }
        };

        let manifest = match this
            .image_store
            .manifest(&repository, &fetching_image_state.manifest_digest)
            .await
        {
            Ok(manifest) => manifest,
            Err(e) => {
                event!(Level::WARN, manifest_error = ?e, "Failed to read image manifest");
                fail!(
                    connector::JobErrorKind::InternalError,
                    format!(
                        "Cannot retrieve image manifest of {}: {e:#}",
                        fetching_image_state.manifest_digest,
                    )
                );
            }
        };

        let image = match parse::parse_image(&manifest) {
            Ok(image) => image,
            Err(e) => {
                event!(Level::WARN, parse_error = %e, "Image manifest is not a valid Treadmill image");
                fail!(
                    connector::JobErrorKind::ImageInvalid,
                    format!(
                        "Image {} is not a valid Treadmill image: {e}",
                        fetching_image_state.manifest_digest,
                    )
                );
            }
        };

        // Place the job in the `ImageFetched` state and continue starting it:
        event!(
            Level::DEBUG,
            ?image,
            "Transitioning job into ImageFetched state"
        );
        *job_lg = QemuSupervisorJobState::ImageFetched(QemuSupervisorImageFetchedState {
            start_job_req: fetching_image_state.start_job_req,
            repository,
            image,
        });

        // Release the lock before continuing to start, otherwise this will
        // deadlock:
        std::mem::drop(job_lg);
        Self::start_job_cont(this, job_id).await
    }

    /// Order the image's runtime backing chain base→head and map each layer to
    /// its on-disk blob path under `repository`.
    ///
    /// The chain is read from the OCI manifest (D3): starting at the head, we
    /// follow each layer's `lower` annotation down to the base, guarding against
    /// dangling references and cycles. Returns the shared read-only lower paths
    /// **base-first** (ready for [`BackingChain::new`]) plus the head layer's
    /// advertised virtual size, used to size the per-job overlay. The backing
    /// paths are never baked into the shared blobs; the chain is assembled at
    /// launch via `-blockdev` nodes (D9).
    #[instrument(skip(self, image), err(Debug, level = Level::WARN))]
    fn assemble_backing_chain(
        &self,
        image: &TreadmillImage,
        repository: &str,
    ) -> Result<(Vec<PathBuf>, u64)> {
        let by_digest: HashMap<&Digest, &ImageLayer> =
            image.layers.iter().map(|l| (&l.digest, l)).collect();

        // Walk head → base following `lower`, collecting head-first.
        let mut head_first: Vec<&ImageLayer> = Vec::with_capacity(image.layers.len());
        let mut seen: std::collections::HashSet<&Digest> = std::collections::HashSet::new();
        let mut cursor = Some(&image.head);
        while let Some(digest) = cursor {
            let layer = by_digest
                .get(digest)
                .ok_or_else(|| anyhow!("backing chain references missing layer {digest}"))?;
            if !seen.insert(digest) {
                bail!("backing chain has a cycle at layer {digest}");
            }
            head_first.push(layer);
            cursor = layer.lower.as_ref();
        }

        let head_virtual_size = head_first
            .first()
            .expect("chain has at least the head layer")
            .virtual_size
            .ok_or_else(|| anyhow!("head layer {} has no virtual-size annotation", image.head))?;

        // The shared read-only lowers, base-first, as direct store blob paths.
        let lower_paths = head_first
            .iter()
            .rev()
            .map(|layer| self.image_store.blob_path(repository, &layer.digest))
            .collect();

        Ok((lower_paths, head_virtual_size))
    }

    /// Create the job's state directory and its per-job writable overlay.
    ///
    /// The overlay is created with **no baked backing** (D3): the lower layers
    /// are supplied at launch as `-blockdev` nodes. It is sized to the
    /// configured working-disk maximum; the head's virtual size must fit within
    /// that ceiling. Returns the job working directory and the overlay path.
    #[instrument(skip(self, start_job_req), err(Debug, level = Level::WARN))]
    async fn allocate_job_disk(
        &self,
        start_job_req: &connector::StartJobMessage,
        head_virtual_size: u64,
    ) -> Result<(PathBuf, PathBuf), connector::JobError> {
        let jobs_dir = self.config.qemu.state_dir.join("jobs");
        let job_dir = jobs_dir.join(start_job_req.job_id.to_string());

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

        // The per-job overlay backs onto the head at launch, so it must be at
        // least as large as the head's virtual size, and no larger than the
        // configured working-disk ceiling (the VM is exposed exactly this size).
        if head_virtual_size > self.config.qemu.working_disk_max_bytes {
            return Err(connector::JobError {
                error_kind: connector::JobErrorKind::ImageInvalid,
                description: format!(
                    "Image head virtual size ({} byte) exceeds the working-disk \
                     maximum ({} byte)",
                    head_virtual_size, self.config.qemu.working_disk_max_bytes,
                ),
            });
        }

        // Create the per-job writable overlay with no baked backing:
        let overlay_file = job_dir.join("overlay.qcow2");
        event!(
            Level::DEBUG,
            ?overlay_file,
            virtual_size_bytes = self.config.qemu.working_disk_max_bytes,
            "Creating per-job overlay disk"
        );
        self.launcher
            .create_overlay_no_backing(&overlay_file, self.config.qemu.working_disk_max_bytes)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Failed to allocate disk image for job {:?}: {:?}",
                    start_job_req.job_id, e,
                ),
            })?;

        Ok((job_dir, overlay_file))
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
            repository,
            image,
        } = match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Starting) {
            QemuSupervisorJobState::ImageFetched(state) => state,
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

        // Order the OCI backing chain base→head and map each layer to its
        // read-only store blob path. The head's virtual size sizes the overlay.
        let (lower_paths, head_virtual_size) =
            match this.assemble_backing_chain(&image, &repository) {
                Ok(v) => v,

                Err(e) => {
                    // The chain is malformed (dangling/cyclic lower, missing
                    // virtual size): treat as an invalid image.
                    this.connector
                        .report_job_error(
                            start_job_req.job_id,
                            connector::JobError {
                                error_kind: connector::JobErrorKind::ImageInvalid,
                                description: format!("Invalid backing chain: {e:#}"),
                            },
                        )
                        .await;

                    // No resources have been allocated (yet), so simply remove
                    // the job and set its state to `Stopping`:
                    this.remove_job(job_id).await;
                    *job_lg = QemuSupervisorJobState::Stopping;

                    return;
                }
            };

        // Allocate the job's working directory and per-job overlay (no baked
        // backing):
        let (job_workdir, overlay_path) = match this
            .allocate_job_disk(&start_job_req, head_virtual_size)
            .await
        {
            Ok(v) => v,

            Err(job_error) => {
                this.connector
                    .report_job_error(start_job_req.job_id, job_error)
                    .await;

                this.remove_job(job_id).await;
                *job_lg = QemuSupervisorJobState::Stopping;

                return;
            }
        };

        // Assemble the runtime backing chain: shared read-only lowers (base
        // first) with the per-job writable overlay on top.
        let chain = BackingChain::new(lower_paths, overlay_path);

        // Start script environment variables / QEMU command line
        // parameter template strings:
        event!(Level::DEBUG, "Templating QEMU argument substitutions");
        let mut qemu_arg_substs: HashMap<String, String> = HashMap::new();
        assert!(
            qemu_arg_substs
                .insert("job_id".to_string(), start_job_req.job_id.to_string())
                .is_none()
        );
        assert!(
            qemu_arg_substs
                .insert("job_workdir".to_string(), job_workdir.display().to_string())
                .is_none()
        );
        // The disk is attached by referencing the writable top node of the
        // backing chain the supervisor prepends as `-blockdev` args below.
        assert!(
            qemu_arg_substs
                .insert("disk_node".to_string(), BackingChain::TOP_NODE.to_string())
                .is_none()
        );

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

        let templated_args = match this
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
                                "Failed to generate QEMU command line arguments: {format_error:?}",
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

        // Prepend the backing-chain `-blockdev` nodes (base → … → overlay) to
        // the configured invocation; the configured args attach the disk device
        // to the writable top node via the `{disk_node}` substitution.
        let mut qemu_args: Vec<String> = Vec::new();
        for node in chain.blockdev_args() {
            qemu_args.push("-blockdev".to_string());
            qemu_args.push(node);
        }
        qemu_args.extend(templated_args);

        // Start a TCP control socket on the specified listen addr:
        let control_socket = TcpControlSocket::new(
            this.config.base.supervisor_id,
            start_job_req.job_id,
            this.config.qemu.tcp_control_socket_listen_addr,
            this.clone(),
        )
        .await
        .unwrap();

        event!(Level::INFO, qemu_binary = ?this.config.qemu.qemu_binary, ?qemu_args, "Launching QEMU process");
        let qemu_proc = this
            .launcher
            .spawn(&this.config.qemu.qemu_binary, &qemu_args, None)
            .await
            .unwrap();

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
            tokio::sync::oneshot::channel::<QemuSupervisorJobRunningState>();

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
        *job_lg = QemuSupervisorJobState::Running(QemuSupervisorJobRunningState {
            start_job_req,
            control_socket,
            shutdown_tx: Some(shutdown_tx),
        });
    }

    async fn process_monitor(
        this: &Arc<Self>,
        job_id: Uuid,
        job: Arc<Mutex<QemuSupervisorJobState>>,
        mut qemu_proc: Box<dyn WorkloadProcess>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<QemuSupervisorJobRunningState>,
    ) {
        let (terminate_message, job_running_state): (String, QemuSupervisorJobRunningState) = loop {
            tokio::select! {
                // When the QEMU process has exited but, at the time of
                // receiving this event the job was no longer in the stopping
                // state, then there must be a message in the shutdown_rx
                // channel. The `stop_job` function will be waiting on this task
                // to exit and move the `job_running_state` back to it.
                //
                // However, if we were to simply always poll for the QEMU exit,
                // we'd be stuck in an infinite loop in this case. Thus, it's
                // important that we check the `shutdown_rx` channel first: if
                // it contains a value, extract it and forward it to the
                // `finish_running_job_shutdown` function. If not, check whether
                // the QEMU process exited on its own.
                //
                // We achieve this polling order by making this select `biased`:
                biased;

                job_running_state_res = &mut shutdown_rx => {
                    let job_running_state = job_running_state_res
                        .expect("Error receving from oneshot shutdown_rx channel");

                    // Received shutdown signal from the stop_job handler
                    // itself, which means that we're asked to kill the QEMU
                    // process:
                    if let Err(e) = qemu_proc.kill().await {
                        // We were unable to terminate the QEMU process:
                        let message = format!("Failed to wait on QEMU process: {e:?}");

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
                        break ("QEMU process was killed.".to_string(), job_running_state);
                    }
                }

                exit_status = qemu_proc.wait() => {
                    event!(
                        Level::DEBUG,
                        ?job_id,
                        "Process monitor task has noticed QEMU process exit"
                    );

                    // QEMU process exited
                    let terminate_message = match exit_status {
                        Ok(status) => {
                            if status.success() {
                                // Notify success
                                "QEMU process exited successfully.".to_string()
                            } else {
                                // Notify failure
                                let message = format!("QEMU process had an internal error with status: {status:?}");

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
                            // Failed to wait on QEMU process
                            let message = format!("Failed to wait on QEMU process: {e:?}");

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
                        "QEMU terminate message: {:?}",
                        terminate_message
                    );

                    // Get a hold of the job lock and extract the job running
                    // state. If it no longer contains a running state, then the
                    // QEMU process exit raced with a call to `stop_job`, which
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

                    match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Stopping) {
                        QemuSupervisorJobState::Running(s) => {
                            break (terminate_message, s);
                        }

                        prev_state => {
                            // Put back the previous state:
                            *job_lg = prev_state;
                            event!(
                                Level::INFO,
                                ?job_id,
                                "Received QEMU process exit, but job is no \
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
        let job: Arc<Mutex<QemuSupervisorJobState>> = {
            self.jobs
                .lock()
                .await
                .get(&job_id)
                .cloned()
                .ok_or(connector::JobError {
                    error_kind: connector::JobErrorKind::JobNotFound,
                    description: format!("Job {job_id:?} not found, cannot stop."),
                })?
        };

        let mut job_lg = job.lock().await;

        enum StopJobPrevState {
            FetchingImage(QemuSupervisorFetchingImageState),
            ImageFetched,
            Running(QemuSupervisorJobRunningState),
        }

        // Make sure the job is in a state in which we can stop it. If so, place
        // it into the Stopping state.
        let prev_job_state = match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Stopping)
        {
            // Stoppable states:
            QemuSupervisorJobState::FetchingImage(s) => StopJobPrevState::FetchingImage(s),
            QemuSupervisorJobState::ImageFetched(_) => StopJobPrevState::ImageFetched,
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
                unreachable!("Job must not be in `Starting state!");
            }

            prev_state @ QemuSupervisorJobState::Stopping => {
                // Put back the previous state:
                *job_lg = prev_state;

                return Err(connector::JobError {
                    error_kind: connector::JobErrorKind::AlreadyStopping,
                    description: format!("Job {job_id:?} is already stopping."),
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
            StopJobPrevState::FetchingImage(QemuSupervisorFetchingImageState {
                poll_task, ..
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

            StopJobPrevState::ImageFetched => {
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
        _job: Arc<Mutex<QemuSupervisorJobState>>,
        terminate_message: String,
        job_running_state: QemuSupervisorJobRunningState,
    ) {
        // When we reach this function, the QEMU process has already
        // exited. Perform the remaining deallocation and shutdown procedures:

        let QemuSupervisorJobRunningState {
            control_socket,
            shutdown_tx: _,
            start_job_req: _,
        } = job_running_state;

        // Shut down the control socket server:
        control_socket.shutdown().await.unwrap();

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

        // Resolve the requested image into its content-addressed digest and
        // the ordered failover list of local-store locations serving it. The
        // supervisor protocol's repository maps directly onto the local Zot
        // daemon's repository in the direct-read model (the daemon's sync
        // config maps it upstream); the `registry` field is for the switchboard
        // catalog (Phase 4/5) and is not consulted here.
        let (manifest_digest, locations) = match start_job_req.image_spec.clone() {
            ImageSpecification::Image {
                manifest_digest,
                locations,
            } => (
                manifest_digest,
                locations
                    .into_iter()
                    .map(|loc| Location::new(loc.repository))
                    .collect::<Vec<_>>(),
            ),
            unsupported_init_spec => {
                unimplemented!("Unsupported init spec: {:?}", unsupported_init_spec)
            }
        };

        // Put the job into the `FetchingImage` state:
        let job_id = start_job_req.job_id; // Copy required below
        *job_lg = QemuSupervisorJobState::FetchingImage(QemuSupervisorFetchingImageState {
            start_job_req,
            manifest_digest,
            locations,
            // Retained for symmetry with the lifecycle; the OCI fetch is a
            // single operation, so no poll task is ever scheduled.
            poll_task: None,
        });

        // Release our lock on the job and hand over to the fetch image method:
        std::mem::drop(job_lg);

        Self::fetch_image(this, job_id).await;

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
impl control_socket::Supervisor for QemuSupervisor {
    #[instrument(skip(self))]
    async fn ssh_keys(&self, _host_id: Uuid, tgt_job_id: Uuid) -> Option<Vec<String>> {
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
                QemuSupervisorJobState::Running(_) => {
                    let hostname = format!("job-{}", format!("{tgt_job_id}").split_at(10).0);
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
                QemuSupervisorJobState::Running(_) => {
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
        // The `Stopping` state is then only set for when the QEMU process is
        // stopped, or when a shutdown is invoked from within the supervisor.
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
        // The `Stopping` state is then only set for when the QEMU process is
        // stopped, or when a shutdown is invoked from within the supervisor.
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
    event!(Level::INFO, "Treadmill Qemu Supervisor, Hello World!");

    let args = QemuSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: QemuSupervisorConfig = toml::from_str(&config_str).unwrap();

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.qemu.qemu_img_binary.clone(),
    ));

    match config.base.coord_connector {
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
                    let connector = Arc::new(treadmill_ws_connector::WsConnector::new(
                        config.base.supervisor_id,
                        ws_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    QemuSupervisor::new(connector, image_store, launcher, args, config)
                })
            };

            let connector = connector_opt.take().unwrap();

            loop {
                if let Err(()) = connector.run().await {
                    warn!("Run method exited with error, trying to reconnect in 1 second...");
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                } else {
                    info!("Run method exited, shutting down supervisor...");
                    break;
                }
            }

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

#[cfg(test)]
mod tests {
    //! In-process drive of the job state machine (plan §12, Phase 0.5).
    //!
    //! With the image store, subprocess launcher, and connector all behind
    //! traits, we can drive `start_job → Ready → Terminated` against stubs —
    //! asserting the reported state transitions and that the workload would have
    //! been launched — without spawning a single real binary.

    use super::*;

    use std::path::Path;
    use std::process::ExitStatus;
    use std::time::Duration;

    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, ParameterValue, RestartPolicy, SupervisorEvent, SupervisorJobEvent,
    };
    use treadmill_rs::connector::{StartJobMessage, StopJobMessage, SupervisorConnector};
    // Bring the trait methods into scope (associated fns / `puppet_ready`)
    // without colliding on the `Supervisor` name:
    use treadmill_rs::connector::Supervisor as _;
    use treadmill_rs::control_socket::Supervisor as _;

    use oci_spec::image::ImageManifest;
    use treadmill_supervisor_lib::launcher::QemuImgMetadata;

    /// Connector that records the job state transitions and errors reported to
    /// it.
    #[derive(Debug, Default)]
    struct RecordingConnector {
        states: std::sync::Mutex<Vec<RunningJobState>>,
        errors: std::sync::Mutex<Vec<connector::JobError>>,
    }

    impl RecordingConnector {
        fn labels(&self) -> Vec<String> {
            self.states.lock().unwrap().iter().map(label).collect()
        }

        fn errors(&self) -> Vec<connector::JobError> {
            self.errors.lock().unwrap().clone()
        }
    }

    fn label(s: &RunningJobState) -> String {
        match s {
            RunningJobState::Initializing { stage } => {
                let stage = match stage {
                    JobInitializingStage::Starting => "starting",
                    JobInitializingStage::FetchingImage => "fetching_image",
                    JobInitializingStage::Allocating => "allocating",
                    JobInitializingStage::Provisioning => "provisioning",
                    JobInitializingStage::Booting => "booting",
                };
                format!("initializing/{stage}")
            }
            RunningJobState::Ready => "ready".to_string(),
            RunningJobState::Terminating => "terminating".to_string(),
            RunningJobState::Terminated => "terminated".to_string(),
        }
    }

    #[async_trait]
    impl SupervisorConnector for RecordingConnector {
        async fn run(&self) -> Result<(), ()> {
            Ok(())
        }

        async fn update_event(&self, event: SupervisorEvent) {
            let SupervisorEvent::JobEvent { event, .. } = event;
            match event {
                SupervisorJobEvent::StateTransition { new_state, .. } => {
                    self.states.lock().unwrap().push(new_state);
                }
                SupervisorJobEvent::Error { error } => {
                    self.errors.lock().unwrap().push(error);
                }
                _ => {}
            }
        }
    }

    /// OCI store stub: returns a canned manifest + a fixed blob path for any
    /// digest/repository, simulating a present image.
    #[derive(Debug)]
    struct StubStore {
        blob_file: PathBuf,
        manifest: ImageManifest,
    }

    #[async_trait]
    impl ImageStore for StubStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<String> {
            Ok("treadmill/stub".to_string())
        }
        async fn manifest(&self, _: &str, _: &Digest) -> Result<ImageManifest> {
            Ok(self.manifest.clone())
        }
        fn blob_path(&self, _: &str, _: &Digest) -> PathBuf {
            self.blob_file.clone()
        }
    }

    /// Launcher that records what it was asked to spawn (instead of spawning
    /// anything) and no-ops the qcow2 operations.
    #[derive(Debug, Default)]
    struct StubLauncher {
        spawned: std::sync::Mutex<Vec<(PathBuf, Vec<String>)>>,
    }

    #[async_trait]
    impl ProcessLauncher for StubLauncher {
        async fn qcow2_info(&self, image: &Path) -> Result<QemuImgMetadata> {
            // Not exercised by the OCI path (the chain is read from the
            // manifest, not qemu-img); return a benign record.
            Ok(QemuImgMetadata {
                filename: image.to_path_buf(),
                virtual_size: 0,
                children: vec![],
                encrypted: None,
                backing_filename_format: None,
                backing_filename: None,
                full_backing_filename: None,
            })
        }

        async fn create_overlay_no_backing(&self, _: &Path, _: u64) -> Result<()> {
            Ok(())
        }

        async fn spawn(
            &self,
            program: &Path,
            args: &[String],
            _cwd: Option<&Path>,
        ) -> Result<Box<dyn WorkloadProcess>> {
            self.spawned
                .lock()
                .unwrap()
                .push((program.to_path_buf(), args.to_vec()));
            Ok(Box::new(StubProcess))
        }
    }

    /// A workload that never exits on its own — it only ends when killed, which
    /// is exactly the path `stop_job` drives.
    struct StubProcess;

    #[async_trait]
    impl WorkloadProcess for StubProcess {
        async fn wait(&mut self) -> std::io::Result<ExitStatus> {
            std::future::pending::<std::io::Result<ExitStatus>>().await
        }
        async fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Canned single-layer OCI manifest digests (any valid sha256 strings).
    const ROOT_DIGEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const EMPTY_DIGEST: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    /// A minimal single-layer Treadmill OCI image manifest the stub serves, with
    /// the head layer advertising `virtual_size`.
    fn single_layer_manifest(virtual_size: u64) -> ImageManifest {
        let json = format!(
            r#"{{
              "schemaVersion": 2,
              "mediaType": "application/vnd.oci.image.manifest.v1+json",
              "artifactType": "application/vnd.treadmill.image.v1+json",
              "config": {{ "mediaType": "application/vnd.oci.empty.v1+json", "digest": "{EMPTY_DIGEST}", "size": 2 }},
              "layers": [
                {{ "mediaType": "application/vnd.treadmill.disk.qcow2", "digest": "{ROOT_DIGEST}", "size": 10,
                   "annotations": {{ "ci.treadmill.role": "root", "ci.treadmill.qcow2.virtual-size": "{virtual_size}" }} }}
              ],
              "annotations": {{ "ci.treadmill.qcow2.head": "{ROOT_DIGEST}" }}
            }}"#
        );
        serde_json::from_str(&json).expect("canned manifest parses as an OCI image manifest")
    }

    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..300 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met within timeout");
    }

    /// A constructed supervisor plus the stubs wired into it, over a temp dir.
    struct Harness {
        sup: Arc<QemuSupervisor>,
        connector: Arc<RecordingConnector>,
        launcher: Arc<StubLauncher>,
        tmp: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    /// Build a supervisor whose stub image's head layer declares
    /// `head_virtual_size`, against a working-disk ceiling of
    /// `working_disk_max_bytes` — equal for a valid image, with the head larger
    /// than the ceiling to provoke an `ImageInvalid` failure.
    fn harness(head_virtual_size: u64, working_disk_max_bytes: u64) -> Harness {
        let tmp = std::env::temp_dir().join(format!("tml-qemu-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let blob_file = tmp.join("root.qcow2");
        std::fs::write(&blob_file, b"not-a-real-qcow2").unwrap();

        let connector = Arc::new(RecordingConnector::default());
        let store: Arc<dyn ImageStore> = Arc::new(StubStore {
            blob_file,
            manifest: single_layer_manifest(head_virtual_size),
        });
        let launcher = Arc::new(StubLauncher::default());

        let config = QemuSupervisorConfig {
            base: SupervisorBaseConfig {
                coord_connector: SupervisorCoordConnector::WsConnector,
                supervisor_id: Uuid::new_v4(),
            },
            ws_connector: None,
            oci_store: OciStoreConfig {
                registry: "127.0.0.1:0".to_string(),
                store_root: tmp.clone(),
            },
            qemu: QemuConfig {
                qemu_binary: PathBuf::from("/nonexistent/qemu"),
                qemu_img_binary: PathBuf::from("/nonexistent/qemu-img"),
                state_dir: tmp.join("state"),
                qemu_args: vec![],
                working_disk_max_bytes,
                tcp_control_socket_listen_addr: "127.0.0.1:0".parse().unwrap(),
                start_script: None,
            },
        };
        let args = QemuSupervisorArgs {
            config_file: PathBuf::new(),
        };

        let sup = Arc::new(QemuSupervisor::new(
            connector.clone(),
            store,
            launcher.clone(),
            args,
            config,
        ));

        Harness {
            sup,
            connector,
            launcher,
            tmp,
        }
    }

    fn start_msg(job_id: Uuid) -> StartJobMessage {
        StartJobMessage {
            job_id,
            image_spec: ImageSpecification::Image {
                manifest_digest: ROOT_DIGEST.parse().unwrap(),
                locations: vec![ImageLocation {
                    registry: "127.0.0.1:0".to_string(),
                    repository: "treadmill/stub".to_string(),
                }],
            },
            ssh_keys: vec![],
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            parameters: HashMap::<String, ParameterValue>::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_lifecycle_transitions() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);

        let job_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();

        // `start_job` drives synchronously through fetch → allocate → launch.
        QemuSupervisor::start_job(&h.sup, start_msg(job_id))
            .await
            .unwrap();

        assert_eq!(
            h.connector.labels(),
            vec![
                "initializing/starting",
                "initializing/allocating",
                "initializing/booting",
            ],
        );

        // The workload was launched with the configured QEMU binary.
        {
            let spawned = h.launcher.spawned.lock().unwrap();
            assert_eq!(spawned.len(), 1);
            assert_eq!(spawned[0].0, PathBuf::from("/nonexistent/qemu"));
        }

        // Puppet reports ready → the job goes Ready.
        h.sup.puppet_ready(0, host_id, job_id).await;
        assert_eq!(
            h.connector.labels().last().map(String::as_str),
            Some("ready")
        );

        // Stopping kills the (stub) workload; the monitor task then reports
        // Terminating → Terminated asynchronously.
        QemuSupervisor::stop_job(&h.sup, StopJobMessage { job_id })
            .await
            .unwrap();

        wait_until(|| h.connector.labels().last().map(String::as_str) == Some("terminated")).await;

        let labels = h.connector.labels();
        assert!(labels.iter().any(|l| l == "terminating"), "{labels:?}");
        assert_eq!(labels.last().map(String::as_str), Some("terminated"));
        assert!(h.connector.errors().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_image_reports_error_and_tears_down() {
        // The image head's virtual size exceeds the working-disk ceiling, so the
        // job fails as ImageInvalid before any workload is launched.
        let h = harness(8 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);

        let job_id = Uuid::new_v4();

        // start_job itself still returns Ok — the failure surfaces as a reported
        // job error, not a returned one.
        QemuSupervisor::start_job(&h.sup, start_msg(job_id))
            .await
            .unwrap();

        let errors = h.connector.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0].error_kind, connector::JobErrorKind::ImageInvalid),
            "{:?}",
            errors[0],
        );

        // Validation failed before launch, and the failed job is gone from the
        // map rather than lingering.
        assert!(h.launcher.spawned.lock().unwrap().is_empty());
        assert!(h.sup.jobs.lock().await.get(&job_id).is_none());

        // It never reached Booting/Ready.
        let labels = h.connector.labels();
        assert!(
            !labels
                .iter()
                .any(|l| l == "initializing/booting" || l == "ready"),
            "{labels:?}",
        );
    }
}
