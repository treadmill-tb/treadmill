use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{Level, event, info, instrument, warn};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageSpecification, JobGatewayDispatch, JobInitializingStage, JobService, LogChannel,
    ParameterValue, RunningJobState,
};

use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::image::Digest;
use treadmill_rs::image::blockdev::BackingChain;
use treadmill_rs::image::parse::{self, ImageLayer, TreadmillImage};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_tcp_control_socket_server::TcpControlSocket;

use treadmill_supervisor_lib::capture::{self, SerialSocket};
use treadmill_supervisor_lib::launcher::{self, ProcessLauncher, StdioMode, WorkloadProcess};
use treadmill_supervisor_lib::oci_store::{ImageStore, Location, OciStore, OciStoreConfig};
use treadmill_supervisor_lib::publisher::{LogPublisher, LogPublisherConfig};

#[derive(Parser, Debug, Clone)]
pub struct QemuSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,

    /// Per-job inputs for the switchboard-less `local` connector
    /// (`coord_connector = "local"`). Ignored by the other connectors.
    #[command(flatten)]
    local_job: Option<treadmill_local_connector::LocalJobArgs>,
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

    // TODO: add tests exercising the stop script, with failures at various
    // parts throughout the job lifecycle
    stop_script: Option<PathBuf>,
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

    /// Local tuning of the console capture→publish path. Optional: omitting
    /// the section leaves every field at its default.
    #[serde(default)]
    log_streaming: LogPublisherConfig,

    qemu: QemuConfig,
}

const PUBLISHER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const JOB_MAILBOX_CAPACITY: usize = 8;

#[derive(Debug, Clone)]
pub enum Phase {
    Starting,
    FetchingImage,
    Allocating,
    Booting,
    Ready,
    Terminating,
    Terminated { outcome: Outcome },
}

impl Phase {
    fn running_job_state(&self) -> RunningJobState {
        match self {
            Phase::Starting => RunningJobState::Initializing {
                stage: JobInitializingStage::Starting,
            },
            Phase::FetchingImage => RunningJobState::Initializing {
                stage: JobInitializingStage::FetchingImage,
            },
            Phase::Allocating => RunningJobState::Initializing {
                stage: JobInitializingStage::Allocating,
            },
            Phase::Booting => RunningJobState::Initializing {
                stage: JobInitializingStage::Booting,
            },
            Phase::Ready => RunningJobState::Ready,
            Phase::Terminating => RunningJobState::Terminating,
            Phase::Terminated { .. } => RunningJobState::Terminated,
        }
    }

    fn terminated(&self) -> bool {
        matches!(self, Phase::Terminated { .. })
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    WorkloadExited(std::process::ExitStatus),
    TerminatedByRequest,
    CancelledDuringStartup,
    Failed(connector::JobError),
}

impl Outcome {
    fn job_error(&self) -> Option<connector::JobError> {
        match self {
            Outcome::Failed(error) => Some(error.clone()),
            Outcome::WorkloadExited(status) if !status.success() => Some(connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("QEMU process had an internal error with status: {status:?}"),
            }),
            _ => None,
        }
    }

    fn status_message(&self) -> String {
        match self {
            Outcome::WorkloadExited(status) if status.success() => {
                "QEMU process exited successfully.".to_string()
            }
            Outcome::WorkloadExited(status) => {
                format!("QEMU process had an internal error with status: {status:?}")
            }
            Outcome::TerminatedByRequest => "QEMU process was killed.".to_string(),
            Outcome::CancelledDuringStartup => "Job terminated while starting up.".to_string(),
            Outcome::Failed(error) => error.description.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobFacts {
    job_id: Uuid,
    phase: Phase,
    parameters: Arc<HashMap<String, ParameterValue>>,
    gateway: Arc<Option<JobGatewayDispatch>>,
    hostname: Arc<str>,
    network_address: Option<IpAddr>,
}

impl JobFacts {
    fn new(start_job_req: &connector::StartJobMessage) -> Self {
        let job_id = start_job_req.job_id;
        JobFacts {
            job_id,
            phase: Phase::Starting,
            parameters: Arc::new(start_job_req.parameters.clone()),
            gateway: Arc::new(start_job_req.gateway.clone()),
            hostname: format!("job-{}", format!("{job_id}").split_at(10).0).into(),
            network_address: None,
        }
    }
}

pub enum JobCommand {
    Terminate {
        ack: oneshot::Sender<Result<(), connector::JobError>>,
    },
    Remove {
        ack: oneshot::Sender<Result<(), connector::JobError>>,
    },
    PuppetReady,
    PuppetTerminate,
    PuppetServiceSet(Vec<JobService>),
}

#[derive(Debug, Clone)]
pub struct JobHandle {
    job_id: Uuid,
    cmd: mpsc::Sender<JobCommand>,
    facts: watch::Receiver<Arc<JobFacts>>,
    cancel: CancellationToken,
}

impl JobHandle {
    fn facts(&self) -> Arc<JobFacts> {
        self.facts.borrow().clone()
    }

    async fn terminate(&self) -> Result<(), connector::JobError> {
        self.cancel.cancel();

        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .cmd
            .send(JobCommand::Terminate { ack: ack_tx })
            .await
            .is_err()
        {
            return Ok(());
        }

        ack_rx.await.unwrap_or(Ok(()))
    }

    async fn remove(&self) -> Result<(), connector::JobError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .cmd
            .send(JobCommand::Remove { ack: ack_tx })
            .await
            .is_err()
        {
            return Ok(());
        }

        ack_rx.await.unwrap_or(Ok(()))
    }

    fn notify(&self, cmd: JobCommand) {
        if self.cmd.try_send(cmd).is_err() {
            event!(
                Level::WARN,
                job_id = ?self.job_id,
                "Dropping a puppet event: the job's mailbox is full or closed",
            );
        }
    }
}

#[derive(Debug)]
struct JobSlot {
    handle: JobHandle,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct JobResources {
    control_socket: Option<TcpControlSocket<QemuSupervisor>>,

    publisher: Option<LogPublisher>,

    workload: Option<Box<dyn WorkloadProcess>>,

    /// Variables associated with this job.
    ///
    /// Generated from default values in start job, can be modified or extended
    /// by the start script, later passed to the stop script.
    job_vars: HashMap<String, String>,

    start_hook_ran: bool,
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

    /// The single job this supervisor runs, occupied from `StartJob` until
    /// `RemoveJob`.
    slot: Mutex<Option<JobSlot>>,

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
            slot: Mutex::new(None),
            _args: args,
            config,
        }
    }

    async fn job_handle(&self, tgt_job_id: Uuid) -> Option<JobHandle> {
        match self.slot.lock().await.as_ref() {
            Some(slot) if slot.handle.job_id == tgt_job_id => Some(slot.handle.clone()),
            _ => {
                event!(
                    Level::WARN,
                    ?tgt_job_id,
                    "Received a puppet request for a job this supervisor does not hold",
                );
                None
            }
        }
    }

    async fn job_facts(&self, tgt_job_id: Uuid) -> Option<Arc<JobFacts>> {
        let facts = self.job_handle(tgt_job_id).await?.facts();
        if facts.phase.terminated() {
            event!(
                Level::WARN,
                ?tgt_job_id,
                "Received a puppet request for a job that has already terminated",
            );
            return None;
        }
        Some(facts)
    }

    /// Order the image's runtime backing chain base→head and map each layer to
    /// its on-disk blob path in the local store.
    ///
    /// The chain is read from the OCI manifest (D3): starting at the head, we
    /// follow each layer's `lower` annotation down to the base, guarding against
    /// dangling references and cycles. Returns the shared read-only lower paths
    /// **base-first** (ready for [`BackingChain::new`]) plus the head layer's
    /// advertised virtual size, used to size the per-job overlay. The backing
    /// paths are never baked into the shared blobs; the chain is assembled at
    /// launch via `-blockdev` nodes (D9).
    #[instrument(skip(self, image), err(Debug, level = Level::WARN))]
    fn assemble_backing_chain(&self, image: &TreadmillImage) -> Result<(Vec<PathBuf>, u64)> {
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
            .ok_or_else(|| anyhow!("backing chain has no layers"))?
            .virtual_size
            .ok_or_else(|| anyhow!("head layer {} has no virtual-size annotation", image.head))?;

        // The shared read-only lowers, base-first, as direct store blob paths.
        let lower_paths = head_first
            .iter()
            .rev()
            .map(|layer| self.image_store.blob_path(&layer.digest))
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

    async fn run_stop_job_script(&self, job_id: Uuid, job_vars: &HashMap<String, String>) {
        if let Some(ref stop_script) = self.config.qemu.stop_script {
            event!(Level::DEBUG, ?stop_script, "Executing stop script");
            let stop_script_res = tokio::process::Command::new(stop_script)
                .stdin(std::process::Stdio::null())
                .envs(
                    job_vars
                        .iter()
                        .map(|(k, v)| (format!("TML_{}", k.to_uppercase()), v)),
                )
                .output()
                .await;

            let stop_script_res = match stop_script_res {
                Err(e) => Err(format!("Failed to spawn stop_script: {}", e)),
                Ok(out) if !out.status.success() => Err(format!(
                    "stop_script exited with {}, stdout: {:?}, stderr: {:?}",
                    out.status, out.stdout, out.stderr
                )),
                Ok(_out) => Ok(()),
            };

            if let Err(description) = stop_script_res {
                // Stop script failed, report an error:
                self.connector
                    .report_job_error(
                        job_id,
                        connector::JobError {
                            error_kind: connector::JobErrorKind::InternalError,
                            description,
                        },
                    )
                    .await;
            }
        }
    }
}

struct JobTask {
    supervisor: Arc<QemuSupervisor>,
    start_job_req: connector::StartJobMessage,
    facts_tx: watch::Sender<Arc<JobFacts>>,
    cancel: CancellationToken,
    resources: JobResources,
    terminate_acks: Vec<oneshot::Sender<Result<(), connector::JobError>>>,
}

impl JobTask {
    fn job_id(&self) -> Uuid {
        self.start_job_req.job_id
    }

    fn update_facts(&self, update: impl FnOnce(&mut JobFacts)) {
        self.facts_tx.send_modify(|facts| {
            let mut next = (**facts).clone();
            update(&mut next);
            *facts = Arc::new(next);
        });
    }

    async fn set_phase(&mut self, phase: Phase) {
        self.supervisor
            .connector
            .update_job_state(self.job_id(), phase.running_job_state(), None)
            .await;
        self.update_facts(|facts| facts.phase = phase);
    }

    #[instrument(skip(self, cmd_rx), fields(job_id = ?self.job_id()))]
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<JobCommand>) {
        self.set_phase(Phase::Starting).await;

        let cancel = self.cancel.clone();
        let startup = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = self.startup() => Some(result),
        };

        let outcome = match startup {
            None => Outcome::CancelledDuringStartup,
            Some(Err(error)) => Outcome::Failed(error),
            Some(Ok(())) => self.supervise(&mut cmd_rx).await,
        };

        self.terminate(outcome).await;
        let remove_ack = self.retain(&mut cmd_rx).await;
        self.release().await;

        if let Some(ack) = remove_ack {
            let _ = ack.send(Ok(()));
        }
    }

    async fn startup(&mut self) -> Result<(), connector::JobError> {
        self.set_phase(Phase::FetchingImage).await;
        let image = self.fetch_and_parse_image().await?;

        self.set_phase(Phase::Allocating).await;

        // Order the OCI backing chain base→head and map each layer to its
        // read-only store blob path. The head's virtual size sizes the overlay.
        //
        // A malformed chain (dangling/cyclic lower, missing virtual size) is
        // treated as an invalid image.
        let (lower_paths, head_virtual_size) = self
            .supervisor
            .assemble_backing_chain(&image)
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::ImageInvalid,
                description: format!("Invalid backing chain: {e:#}"),
            })?;

        // Allocate the job's working directory and per-job overlay (no baked
        // backing):
        let (job_workdir, overlay_path) = self
            .supervisor
            .allocate_job_disk(&self.start_job_req, head_virtual_size)
            .await?;

        // Assemble the runtime backing chain: shared read-only lowers (base
        // first) with the per-job writable overlay on top.
        let chain = BackingChain::new(lower_paths, overlay_path);

        // Variables that can be produced by the start script, and used for
        // templating the QEMU cmd string or setting other job-specific values
        // (e.g., the host IP), populated with default values like the Job ID,
        // working directory or disk node.
        let job_vars = &mut self.resources.job_vars;
        job_vars.insert("job_id".to_string(), self.start_job_req.job_id.to_string());
        job_vars.insert("job_workdir".to_string(), job_workdir.display().to_string());
        // The disk is attached by referencing the writable top node of the
        // backing chain the supervisor prepends as `-blockdev` args below.
        job_vars.insert("disk_node".to_string(), BackingChain::TOP_NODE.to_string());

        self.run_start_job_script().await?;

        let templated_args = self
            .supervisor
            .config
            .qemu
            .qemu_args
            .iter()
            .map(|argstr| strfmt::strfmt(argstr, &self.resources.job_vars))
            .collect::<Result<Vec<String>, strfmt::FmtError>>()
            .map_err(|format_error| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Failed to generate QEMU command line arguments: {format_error:?}",
                ),
            })?;

        // Prepend the backing-chain `-blockdev` nodes (base → … → overlay) to
        // the configured invocation; the configured args attach the disk device
        // to the writable top node via the `{disk_node}` substitution.
        let mut qemu_args: Vec<String> = Vec::new();
        for node in chain.blockdev_args() {
            qemu_args.push("-blockdev".to_string());
            qemu_args.push(node);
        }
        qemu_args.extend(templated_args);

        // When the dispatch enabled log streaming, capture qemu's console
        // output: pipe stdout/stderr (read back below) and route the guest
        // serial console to a unix socket we own. When it's disabled, keep the
        // historical behavior — stdout/stderr inherit our terminal and the
        // serial console goes wherever the configured args point it.
        let (stdio_mode, serial_socket) = if self.start_job_req.log_streaming.is_some() {
            let serial_sock_path = job_workdir.join("serial.sock");
            match SerialSocket::bind(&serial_sock_path).await {
                Ok(socket) => {
                    // qemu connects to our already-bound listener as the client
                    // (`server=off`), so there is no connect race.
                    qemu_args.push("-chardev".to_string());
                    qemu_args.push(format!(
                        "socket,id=tml-serial,path={},server=off",
                        socket.path().display(),
                    ));
                    qemu_args.push("-serial".to_string());
                    qemu_args.push("chardev:tml-serial".to_string());
                    (StdioMode::Capture, Some(socket))
                }
                Err(e) => {
                    // Don't fail the job over a capture-setup error; fall back
                    // to inheriting and skip the serial channel.
                    event!(
                        Level::WARN,
                        ?serial_sock_path,
                        error = ?e,
                        "Failed to bind serial capture socket; disabling log capture for this job",
                    );
                    (StdioMode::Inherit, None)
                }
            }
        } else {
            (StdioMode::Inherit, None)
        };

        // Start a TCP control socket on the specified listen addr:
        let listen_addr = self.supervisor.config.qemu.tcp_control_socket_listen_addr;
        let control_socket = TcpControlSocket::new(
            self.supervisor.config.base.supervisor_id,
            self.job_id(),
            listen_addr,
            self.supervisor.clone(),
        )
        .await
        .map_err(|e| connector::JobError {
            error_kind: connector::JobErrorKind::InternalError,
            description: format!("Failed to bind the control socket at {listen_addr:?}: {e:#}"),
        })?;
        self.resources.control_socket = Some(control_socket);

        event!(
            Level::INFO,
            qemu_binary = ?self.supervisor.config.qemu.qemu_binary,
            ?qemu_args,
            "Launching QEMU process",
        );
        let mut workload = self
            .supervisor
            .launcher
            .spawn(
                &self.supervisor.config.qemu.qemu_binary,
                &qemu_args,
                None,
                stdio_mode,
            )
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Failed to launch the QEMU process: {e:#}"),
            })?;

        // Ship the captured console channels to NATS (durable spill + ack +
        // resume). Takes the stdout/stderr readers before the process is handed
        // to `supervise`. Spill files live under the per-job workdir so they
        // survive a supervisor restart and are retained for post-mortem after
        // the job ends.
        if let Some(dispatch) = self.start_job_req.log_streaming.clone() {
            let stdout = workload.take_stdout();
            let stderr = workload.take_stderr();
            let spill_dir = job_workdir.join("logs");
            let config = self.supervisor.config.log_streaming.clone();
            match LogPublisher::connect(&dispatch, spill_dir, config).await {
                Ok(publisher) => {
                    if let Some(stdout) = stdout {
                        publisher.spawn_channel(LogChannel::QemuStdout, stdout);
                    }
                    if let Some(stderr) = stderr {
                        publisher.spawn_channel(LogChannel::QemuStderr, stderr);
                    }
                    if let Some(socket) = serial_socket {
                        publisher.spawn_serial(LogChannel::Serial, socket);
                    }
                    self.resources.publisher = Some(publisher);
                }
                Err(e) => {
                    // Don't fail the job over log-streaming setup; fall back to
                    // draining capture to our terminal so the qemu pipes don't
                    // block and the operator still sees output.
                    event!(
                        Level::WARN,
                        error = ?e,
                        "Failed to start log publisher; draining capture to terminal instead",
                    );
                    capture::drain_to_stdio(stdout, stderr, serial_socket);
                }
            }
        }

        self.resources.workload = Some(workload);

        // Booting, but puppet has not yet reported "ready":
        self.set_phase(Phase::Booting).await;

        self.report_job_address().await;

        Ok(())
    }

    /// Resolve the dispatched image into the local OCI store: ask it to make
    /// the manifest digest present — a copy from one of the dispatched
    /// locations, or a cache hit — then read+parse its manifest into the
    /// Treadmill backing-chain view.
    async fn fetch_and_parse_image(&mut self) -> Result<TreadmillImage, connector::JobError> {
        let (manifest_digest, locations) = match &self.start_job_req.image_spec {
            ImageSpecification::Image {
                manifest_digest,
                locations,
            } => (
                *manifest_digest,
                locations
                    .iter()
                    .cloned()
                    .map(|loc| Location::new(loc.registry, loc.repository))
                    .collect::<Vec<_>>(),
            ),

            unsupported_image_spec => {
                return Err(connector::JobError {
                    error_kind: connector::JobErrorKind::ImageNotCompatible,
                    description: format!(
                        "Unsupported image specification: {unsupported_image_spec:?}",
                    ),
                });
            }
        };

        event!(
            Level::TRACE,
            %manifest_digest,
            ?locations,
            "Ensuring image present in the local OCI store",
        );

        self.supervisor
            .image_store
            .ensure_present(&manifest_digest, &locations)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Failed to fetch image {manifest_digest}: {e:#}"),
            })?;

        let manifest = self
            .supervisor
            .image_store
            .manifest(&manifest_digest)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Cannot retrieve image manifest of {manifest_digest}: {e:#}",),
            })?;

        parse::parse_image(&manifest).map_err(|e| connector::JobError {
            error_kind: connector::JobErrorKind::ImageInvalid,
            description: format!("Image {manifest_digest} is not a valid Treadmill image: {e}"),
        })
    }

    async fn run_start_job_script(&mut self) -> Result<(), connector::JobError> {
        let Some(start_script) = self.supervisor.config.qemu.start_script.clone() else {
            return Ok(());
        };

        event!(Level::DEBUG, ?start_script, "Executing start script");

        // Even if the start_script fails to spawn or errors midway through we
        // still give the stop_script a chance to clean up resources:
        self.resources.start_hook_ran = true;

        let start_script_res = tokio::process::Command::new(&start_script)
            .stdin(std::process::Stdio::null())
            .envs(
                self.resources
                    .job_vars
                    .iter()
                    .map(|(k, v)| (format!("TML_{}", k.to_uppercase()), v)),
            )
            .output()
            .await;

        let start_script_out = match start_script_res {
            Err(e) => Err(format!("Failed to spawn start_script: {}", e)),
            Ok(out) if !out.status.success() => Err(format!(
                "start_script exited with {}, stdout: {:?}, stderr: {:?}",
                out.status, out.stdout, out.stderr
            )),
            Ok(out) => Ok(out),
        }
        .map_err(|description| connector::JobError {
            error_kind: connector::JobErrorKind::InternalError,
            description,
        })?;

        let Ok(stdout) = std::str::from_utf8(&start_script_out.stdout) else {
            event!(
                Level::WARN,
                stdout = %String::from_utf8_lossy(&start_script_out.stdout),
                "Start script produced non-UTF8 characters on standard output, refusing to interpret",
            );
            return Ok(());
        };

        for line in stdout.lines() {
            let Some(key_value) = line.strip_prefix("tml-set-variable:") else {
                continue;
            };
            match key_value.split_once('=') {
                Some((key, value)) => {
                    event!(
                        Level::DEBUG,
                        key,
                        value,
                        "Extracted variable {key:?} from start script output",
                    );
                    self.resources
                        .job_vars
                        .insert(key.to_string(), value.to_string());
                }
                None => event!(
                    Level::WARN,
                    command = line,
                    "Malformed tml-set-variable command"
                ),
            }
        }

        Ok(())
    }

    /// Determine the job's IP address and report it. It can either be set as a
    /// static IP in the configuration file (taking priority), or be set by the
    /// start_script.
    async fn report_job_address(&mut self) {
        let mut job_address = self.supervisor.config.base.job_address;
        if job_address.is_none()
            && let Some(job_address_str) = self.resources.job_vars.get("job_ip_address")
        {
            job_address = <IpAddr as FromStr>::from_str(job_address_str)
                .inspect_err(|e| event!(
                    Level::WARN,
                    error = ?e,
                    "Failed to parse `job_ip_address` variable from start script, not reporting",
                ))
                .ok();
        }

        let Some(job_address) = job_address else {
            return;
        };

        self.update_facts(|facts| facts.network_address = Some(job_address));
        self.supervisor
            .connector
            .report_job_network_address(self.job_id(), job_address)
            .await;
    }

    async fn supervise(&mut self, cmd_rx: &mut mpsc::Receiver<JobCommand>) -> Outcome {
        let Some(mut workload) = self.resources.workload.take() else {
            return Outcome::Failed(connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: "The job reached `supervise` without a workload process.".to_string(),
            });
        };

        enum Wake {
            Command(Option<JobCommand>),
            Exited(std::io::Result<std::process::ExitStatus>),
        }

        loop {
            let wake = tokio::select! {
                biased;
                cmd = cmd_rx.recv() => Wake::Command(cmd),
                exit_status = workload.wait() => Wake::Exited(exit_status),
            };

            match wake {
                Wake::Exited(Ok(status)) => return Outcome::WorkloadExited(status),

                Wake::Exited(Err(e)) => {
                    self.resources.workload = Some(workload);
                    return Outcome::Failed(connector::JobError {
                        error_kind: connector::JobErrorKind::InternalError,
                        description: format!("Failed to wait on QEMU process: {e:?}"),
                    });
                }

                Wake::Command(None) => {
                    self.resources.workload = Some(workload);
                    return Outcome::TerminatedByRequest;
                }

                Wake::Command(Some(JobCommand::Terminate { ack })) => {
                    self.terminate_acks.push(ack);
                    self.resources.workload = Some(workload);
                    return Outcome::TerminatedByRequest;
                }

                Wake::Command(Some(JobCommand::PuppetTerminate)) => {
                    self.resources.workload = Some(workload);
                    return Outcome::TerminatedByRequest;
                }

                Wake::Command(Some(JobCommand::Remove { ack })) => {
                    let _ = ack.send(Err(connector::JobError {
                        error_kind: connector::JobErrorKind::NotTerminated,
                        description: format!(
                            "Job {:?} is still executing and must be terminated before removal.",
                            self.job_id(),
                        ),
                    }));
                }

                Wake::Command(Some(JobCommand::PuppetReady)) => {
                    self.set_phase(Phase::Ready).await;
                }

                Wake::Command(Some(JobCommand::PuppetServiceSet(services))) => {
                    self.supervisor
                        .connector
                        .report_job_service_set(self.job_id(), services)
                        .await;
                }
            }
        }
    }

    async fn terminate(&mut self, outcome: Outcome) {
        if !matches!(outcome, Outcome::Failed(_)) {
            self.set_phase(Phase::Terminating).await;
        }

        if let Some(mut workload) = self.resources.workload.take()
            && let Err(e) = workload.kill().await
        {
            event!(Level::WARN, error = ?e, "Failed to kill the QEMU process");
        }

        if let Some(error) = outcome.job_error() {
            self.supervisor
                .connector
                .report_job_error(self.job_id(), error)
                .await;
        }

        let status_message = outcome.status_message();
        self.set_phase(Phase::Terminated { outcome }).await;
        event!(Level::INFO, status_message, "Job terminated");

        for ack in self.terminate_acks.drain(..) {
            let _ = ack.send(Ok(()));
        }
    }

    async fn retain(
        &mut self,
        cmd_rx: &mut mpsc::Receiver<JobCommand>,
    ) -> Option<oneshot::Sender<Result<(), connector::JobError>>> {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                JobCommand::Remove { ack } => return Some(ack),

                JobCommand::Terminate { ack } => {
                    let _ = ack.send(Ok(()));
                }

                JobCommand::PuppetReady
                | JobCommand::PuppetTerminate
                | JobCommand::PuppetServiceSet(_) => (),
            }
        }

        None
    }

    async fn release(&mut self) {
        let JobResources {
            control_socket,
            publisher,
            workload: _,
            job_vars,
            start_hook_ran,
        } = std::mem::take(&mut self.resources);

        if let Some(control_socket) = control_socket
            && let Err(e) = control_socket.shutdown().await
        {
            event!(Level::WARN, error = ?e, "Failed to shut down the control socket");
        }

        if start_hook_ran {
            self.supervisor
                .run_stop_job_script(self.job_id(), &job_vars)
                .await;
        }

        if let Some(publisher) = publisher {
            publisher.drain(PUBLISHER_DRAIN_TIMEOUT).await;
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

        let mut slot_lg = this.slot.lock().await;

        if let Some(slot) = slot_lg.as_ref() {
            let facts = slot.handle.facts();
            return Err(if facts.job_id == start_job_req.job_id {
                connector::JobError {
                    error_kind: connector::JobErrorKind::JobAlreadyExists,
                    description: format!(
                        "Job {:?} already occupies this supervisor's job slot.",
                        facts.job_id,
                    ),
                }
            } else if facts.phase.terminated() {
                connector::JobError {
                    error_kind: connector::JobErrorKind::MaxConcurrentJobs,
                    description: format!(
                        "Supervisor {:?} still retains the terminated job {:?}, which has to be \
                         removed before another job can be started.",
                        this.config.base.supervisor_id, facts.job_id,
                    ),
                }
            } else {
                connector::JobError {
                    error_kind: connector::JobErrorKind::AlreadyRunning,
                    description: format!(
                        "Supervisor {:?} is already running job {:?}.",
                        this.config.base.supervisor_id, facts.job_id,
                    ),
                }
            });
        }

        let job_id = start_job_req.job_id;
        let (cmd_tx, cmd_rx) = mpsc::channel(JOB_MAILBOX_CAPACITY);
        let (facts_tx, facts_rx) = watch::channel(Arc::new(JobFacts::new(&start_job_req)));
        let cancel = CancellationToken::new();

        let task = JobTask {
            supervisor: this.clone(),
            start_job_req,
            facts_tx,
            cancel: cancel.clone(),
            resources: JobResources::default(),
            terminate_acks: Vec::new(),
        };

        *slot_lg = Some(JobSlot {
            handle: JobHandle {
                job_id,
                cmd: cmd_tx,
                facts: facts_rx,
                cancel,
            },
            task: tokio::spawn(task.run(cmd_rx)),
        });

        Ok(())
    }

    #[instrument(skip(this), err(Debug, level = Level::WARN))]
    async fn terminate_job(
        this: &Arc<Self>,
        msg: connector::TerminateJobMessage,
    ) -> Result<(), connector::JobError> {
        let handle = match this.slot.lock().await.as_ref() {
            Some(slot) if slot.handle.job_id == msg.job_id => slot.handle.clone(),
            _ => return Ok(()),
        };

        handle.terminate().await
    }

    #[instrument(skip(this), err(Debug, level = Level::WARN))]
    async fn remove_job(
        this: &Arc<Self>,
        msg: connector::RemoveJobMessage,
    ) -> Result<(), connector::JobError> {
        let handle = match this.slot.lock().await.as_ref() {
            Some(slot) if slot.handle.job_id == msg.job_id => slot.handle.clone(),
            _ => return Ok(()),
        };

        if !handle.facts().phase.terminated() {
            return Err(connector::JobError {
                error_kind: connector::JobErrorKind::NotTerminated,
                description: format!(
                    "Job {:?} is still executing and must be terminated before removal.",
                    msg.job_id,
                ),
            });
        }

        handle.remove().await?;

        let slot = this.slot.lock().await.take();
        if let Some(slot) = slot {
            let _ = slot.task.await;
        }

        Ok(())
    }
}

#[async_trait]
impl control_socket::Supervisor for QemuSupervisor {
    #[instrument(skip(self))]
    async fn network_config(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::NetworkConfig> {
        let facts = self.job_facts(tgt_job_id).await?;
        Some(treadmill_rs::api::supervisor_puppet::NetworkConfig {
            hostname: facts.hostname.to_string(),
            // QemuSupervisor, don't supply a network interface to configure:
            interface: None,
            ipv4: None,
            ipv6: None,
        })
    }

    #[instrument(skip(self))]
    async fn parameters(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<HashMap<String, ParameterValue>> {
        let facts = self.job_facts(tgt_job_id).await?;
        Some((*facts.parameters).clone())
    }

    #[instrument(skip(self))]
    async fn gateway(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::JobGatewayInfo> {
        let facts = self.job_facts(tgt_job_id).await?;

        // Hand back whatever the coordinator dispatched this job with:
        facts.gateway.as_ref().as_ref().map(|gateway| {
            treadmill_rs::api::supervisor_puppet::JobGatewayInfo {
                issuer: gateway.issuer.clone(),
                signing_public_key: gateway.signing_public_key.clone(),
                key_id: gateway.key_id.clone(),
                endpoints: gateway
                    .endpoints
                    .iter()
                    .cloned()
                    .map(
                        |treadmill_rs::api::switchboard_supervisor::JobGatewayEndpoint {
                             base_domain,
                             port,
                         }| {
                            treadmill_rs::api::supervisor_puppet::JobGatewayEndpoint {
                                base_domain,
                                port,
                            }
                        },
                    )
                    .collect(),
            }
        })
    }

    #[instrument(skip(self))]
    async fn puppet_ready(&self, _puppet_event_id: u64, _host_id: Uuid, job_id: Uuid) {
        event!(Level::INFO, "Received puppet ready event");

        if let Some(handle) = self.job_handle(job_id).await {
            handle.notify(JobCommand::PuppetReady);
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

        // See `puppet_shutdown`: a puppet-reported reboot is not a supervisor
        // state transition.
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

        if let Some(handle) = self.job_handle(job_id).await {
            handle.cancel.cancel();
            handle.notify(JobCommand::PuppetTerminate);
        }
    }

    #[instrument(skip(self))]
    async fn job_service_set(
        &self,
        _puppet_event_id: u64,
        services: Vec<JobService>,
        _host_id: Uuid,
        job_id: Uuid,
    ) {
        event!(
            Level::INFO,
            ?job_id,
            "Received puppet event announcing {} job service(s)",
            services.len(),
        );

        if let Some(handle) = self.job_handle(job_id).await {
            handle.notify(JobCommand::PuppetServiceSet(services));
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
        SupervisorCoordConnector::Local => {
            // One-shot, switchboard-less run: drive a single job from the
            // command-line `LocalJobArgs` against the local OCI store.
            let local_job = args.local_job.clone().ok_or(anyhow!(
                "Requested the `local` connector, but no job was supplied on the \
                 command line (need at least --manifest-digest and --repository)."
            ))?;
            if local_job.manifest_digest.is_none() || local_job.repository.is_none() {
                bail!("The `local` connector requires both --manifest-digest and --repository.");
            }
            let registry = config.oci_store.registry.clone();

            // Same cyclic Arc dance as the WsConnector arm: connector and
            // supervisor reference each other.
            let mut connector_opt = None;

            let qemu_supervisor = {
                let connector_opt = &mut connector_opt;
                Arc::new_cyclic(move |weak_supervisor| {
                    let connector = Arc::new(treadmill_local_connector::LocalConnector::new(
                        registry,
                        local_job,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    QemuSupervisor::new(connector, image_store, launcher, args, config)
                })
            };

            let connector = connector_opt.take().unwrap();

            // Ctrl-C requests a graceful shutdown: stop the job and let run()
            // return. A second Ctrl-C (after run() has returned) terminates the
            // process the usual way.
            let connector_for_signal = connector.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("Received Ctrl-C => requesting graceful shutdown...");
                    connector_for_signal.request_shutdown();
                }
            });

            if let Err(()) = connector.run().await {
                warn!("Local run exited with an error.");
            } else {
                info!("Local run finished, shutting down supervisor...");
            }

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

    use treadmill_rs::api;
    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, JobGatewayDispatch, JobService, ParameterValue, RestartPolicy,
        SupervisorEvent, SupervisorJobEvent,
    };
    use treadmill_rs::connector::{
        RemoveJobMessage, StartJobMessage, SupervisorConnector, TerminateJobMessage,
    };
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
        addresses: std::sync::Mutex<Vec<std::net::IpAddr>>,
        service_sets: std::sync::Mutex<Vec<Vec<JobService>>>,
    }

    impl RecordingConnector {
        fn labels(&self) -> Vec<String> {
            self.states.lock().unwrap().iter().map(label).collect()
        }

        fn errors(&self) -> Vec<connector::JobError> {
            self.errors.lock().unwrap().clone()
        }

        fn addresses(&self) -> Vec<std::net::IpAddr> {
            self.addresses.lock().unwrap().clone()
        }

        fn service_sets(&self) -> Vec<Vec<JobService>> {
            self.service_sets.lock().unwrap().clone()
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
                SupervisorJobEvent::JobNetworkAddress { address } => {
                    self.addresses.lock().unwrap().push(address);
                }
                SupervisorJobEvent::JobServiceSet { services } => {
                    self.service_sets.lock().unwrap().push(services);
                }
                _ => {}
            }
        }
    }

    /// OCI store stub: returns a canned manifest + a fixed blob path for any
    /// digest, simulating a present image.
    #[derive(Debug)]
    struct StubStore {
        blob_file: PathBuf,
        manifest: ImageManifest,
    }

    #[async_trait]
    impl ImageStore for StubStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<()> {
            Ok(())
        }
        async fn manifest(&self, _: &Digest) -> Result<ImageManifest> {
            Ok(self.manifest.clone())
        }
        fn blob_path(&self, _: &Digest) -> PathBuf {
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
            _stdio: StdioMode,
        ) -> Result<Box<dyn WorkloadProcess>> {
            self.spawned
                .lock()
                .unwrap()
                .push((program.to_path_buf(), args.to_vec()));
            Ok(Box::new(StubProcess))
        }
    }

    /// A workload that never exits on its own — it only ends when killed, which
    /// is exactly the path `terminate_job` drives.
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
                   "annotations": {{ "dev.treadmill.role": "root", "dev.treadmill.qcow2.virtual-size": "{virtual_size}" }} }}
              ],
              "annotations": {{ "dev.treadmill.qcow2.head": "{ROOT_DIGEST}" }}
            }}"#
        );
        serde_json::from_str(&json).expect("canned manifest parses as an OCI image manifest")
    }

    async fn job_facts(sup: &Arc<QemuSupervisor>) -> watch::Receiver<Arc<JobFacts>> {
        sup.slot
            .lock()
            .await
            .as_ref()
            .expect("a job occupies the slot")
            .handle
            .facts
            .clone()
    }

    async fn wait_for(
        facts: &mut watch::Receiver<Arc<JobFacts>>,
        reached: impl Fn(&JobFacts) -> bool,
    ) {
        loop {
            if reached(&facts.borrow_and_update()) {
                return;
            }
            facts
                .changed()
                .await
                .expect("the job task keeps publishing its facts");
        }
    }

    fn booting(facts: &JobFacts) -> bool {
        matches!(facts.phase, Phase::Booting)
    }

    fn ready(facts: &JobFacts) -> bool {
        matches!(facts.phase, Phase::Ready)
    }

    fn terminated(facts: &JobFacts) -> bool {
        facts.phase.terminated()
    }

    async fn start_and_boot(h: &Harness, msg: StartJobMessage) -> watch::Receiver<Arc<JobFacts>> {
        let job_id = msg.job_id;
        QemuSupervisor::start_job(&h.sup, msg).await.unwrap();

        let mut facts = job_facts(&h.sup).await;
        wait_for(&mut facts, booting).await;

        h.sup.puppet_ready(0, Uuid::new_v4(), job_id).await;
        wait_for(&mut facts, ready).await;

        facts
    }

    async fn terminate(sup: &Arc<QemuSupervisor>, job_id: Uuid) -> Result<(), connector::JobError> {
        <QemuSupervisor as connector::Supervisor>::terminate_job(
            sup,
            TerminateJobMessage { job_id },
        )
        .await
    }

    async fn remove(sup: &Arc<QemuSupervisor>, job_id: Uuid) -> Result<(), connector::JobError> {
        <QemuSupervisor as connector::Supervisor>::remove_job(sup, RemoveJobMessage { job_id })
            .await
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
    /// than the ceiling to provoke an `ImageInvalid` failure. Configures no
    /// job address, as a deployment without a gateway has none.
    fn harness(head_virtual_size: u64, working_disk_max_bytes: u64) -> Harness {
        harness_with(head_virtual_size, working_disk_max_bytes, None, false, None)
    }

    /// Like [`harness`], for a supervisor reading from `store` instead of the
    /// always-present [`StubStore`].
    fn harness_with_store(head_virtual_size: u64, store: Arc<dyn ImageStore>) -> Harness {
        harness_with(
            head_virtual_size,
            head_virtual_size,
            None,
            false,
            Some(store),
        )
    }

    /// Like [`harness`], for a supervisor configured to report `job_address`
    /// for the jobs it runs.
    fn harness_with_job_address(
        head_virtual_size: u64,
        working_disk_max_bytes: u64,
        job_address: Option<std::net::IpAddr>,
    ) -> Harness {
        harness_with(
            head_virtual_size,
            working_disk_max_bytes,
            job_address,
            false,
            None,
        )
    }

    /// Like [`harness`], for a supervisor configured with start and stop
    /// scripts that each append a line to `<tmp>/<start|stop>-hook.log`, so a
    /// test can count how often they ran (see [`hook_runs`]).
    fn harness_with_hooks(head_virtual_size: u64, working_disk_max_bytes: u64) -> Harness {
        harness_with(head_virtual_size, working_disk_max_bytes, None, true, None)
    }

    /// Count the lines the named hook appended, zero if it never ran.
    fn hook_runs(tmp: &Path, hook: &str) -> usize {
        std::fs::read_to_string(tmp.join(format!("{hook}-hook.log")))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    /// Write a hook script appending one line to `<tmp>/<hook>-hook.log`.
    fn write_hook(tmp: &Path, hook: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = tmp.join(format!("{hook}-hook.sh"));
        let log = tmp.join(format!("{hook}-hook.log"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho ran >> {}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn harness_with(
        head_virtual_size: u64,
        working_disk_max_bytes: u64,
        job_address: Option<std::net::IpAddr>,
        hooks: bool,
        store: Option<Arc<dyn ImageStore>>,
    ) -> Harness {
        let tmp = std::env::temp_dir().join(format!("tml-qemu-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let blob_file = tmp.join("root.qcow2");
        std::fs::write(&blob_file, b"not-a-real-qcow2").unwrap();

        let connector = Arc::new(RecordingConnector::default());
        let store: Arc<dyn ImageStore> = store.unwrap_or_else(|| {
            Arc::new(StubStore {
                blob_file,
                manifest: single_layer_manifest(head_virtual_size),
            })
        });
        let launcher = Arc::new(StubLauncher::default());

        let config = QemuSupervisorConfig {
            base: SupervisorBaseConfig {
                coord_connector: SupervisorCoordConnector::WsConnector,
                supervisor_id: Uuid::new_v4(),
                job_address,
            },
            ws_connector: None,
            oci_store: OciStoreConfig {
                registry: "127.0.0.1:0".to_string(),
                store_root: tmp.clone(),
            },
            log_streaming: LogPublisherConfig::default(),
            qemu: QemuConfig {
                qemu_binary: PathBuf::from("/nonexistent/qemu"),
                qemu_img_binary: PathBuf::from("/nonexistent/qemu-img"),
                state_dir: tmp.join("state"),
                qemu_args: vec![],
                working_disk_max_bytes,
                tcp_control_socket_listen_addr: "127.0.0.1:0".parse().unwrap(),
                start_script: hooks.then(|| write_hook(&tmp, "start")),
                stop_script: hooks.then(|| write_hook(&tmp, "stop")),
            },
        };
        let args = QemuSupervisorArgs {
            config_file: PathBuf::new(),
            local_job: None,
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
        start_msg_with_gateway(job_id, None)
    }

    /// Like [`start_msg`], for a job the coordinator dispatched with gateway
    /// material for the supervisor to relay into it.
    fn start_msg_with_gateway(
        job_id: Uuid,
        gateway: Option<JobGatewayDispatch>,
    ) -> StartJobMessage {
        StartJobMessage {
            job_id,
            image_spec: ImageSpecification::Image {
                manifest_digest: ROOT_DIGEST.parse().unwrap(),
                locations: vec![ImageLocation {
                    registry: "127.0.0.1:0".to_string(),
                    repository: "treadmill/stub".to_string(),
                }],
            },
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            parameters: HashMap::<String, ParameterValue>::new(),
            log_streaming: None,
            gateway,
        }
    }

    /// An [`ImageStore`] whose `ensure_present` blocks until the test releases
    /// it, so a job can be stopped while it is still fetching its image.
    #[derive(Debug)]
    struct GatedStore {
        inner: StubStore,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ImageStore for GatedStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<()> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
        async fn manifest(&self, digest: &Digest) -> Result<ImageManifest> {
            self.inner.manifest(digest).await
        }
        fn blob_path(&self, digest: &Digest) -> PathBuf {
            self.inner.blob_path(digest)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_lifecycle_transitions() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);

        let job_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();

        QemuSupervisor::start_job(&h.sup, start_msg(job_id))
            .await
            .unwrap();
        let mut facts = job_facts(&h.sup).await;
        wait_for(&mut facts, booting).await;

        assert_eq!(
            h.connector.labels(),
            vec![
                "initializing/starting",
                "initializing/fetching_image",
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
        wait_for(&mut facts, ready).await;
        assert_eq!(
            h.connector.labels().last().map(String::as_str),
            Some("ready")
        );

        // Terminating kills the (stub) workload and reports the terminal
        // transition before it returns.
        terminate(&h.sup, job_id).await.unwrap();

        let labels = h.connector.labels();
        assert!(labels.iter().any(|l| l == "terminating"), "{labels:?}");
        assert_eq!(labels.last().map(String::as_str), Some("terminated"));
        assert!(h.connector.errors().is_empty());

        // The record is retained until it is removed.
        assert!(terminated(&facts.borrow_and_update()));
        remove(&h.sup, job_id).await.unwrap();
        assert!(h.sup.slot.lock().await.is_none());
    }

    /// A supervisor configured with a job address reports it as the job starts,
    /// so the coordinator has somewhere to point a gateway at before the job is
    /// up. One without stays silent, and the job is reachable from nowhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_configured_job_address_is_reported_at_start() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let address: std::net::IpAddr = "fd00::2".parse().unwrap();
        let h = harness_with_job_address(virtual_size, virtual_size, Some(address));

        assert!(h.connector.addresses().is_empty(), "nothing has started");

        start_and_boot(&h, start_msg(Uuid::new_v4())).await;
        assert_eq!(h.connector.addresses(), vec![address]);

        let unconfigured = harness(virtual_size, virtual_size);
        start_and_boot(&unconfigured, start_msg(Uuid::new_v4())).await;
        assert!(unconfigured.connector.addresses().is_empty());
    }

    /// The puppet asks its supervisor what to validate service tokens against,
    /// and gets back exactly what the coordinator dispatched the job with —
    /// nothing if the job was dispatched without a gateway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_job_is_told_its_gateway_material() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let host_id = Uuid::new_v4();
        let dispatched = JobGatewayDispatch {
            issuer: "https://switchboard.example".to_string(),
            signing_public_key: "-----BEGIN PUBLIC KEY-----\nstub\n-----END PUBLIC KEY-----\n"
                .to_string(),
            key_id: "wI9c-yvsF8".to_string(),
            endpoints: vec![
                api::switchboard_supervisor::JobGatewayEndpoint {
                    base_domain: "gw-us-east-1.treadmillusercontent.com".to_string(),
                    port: 443,
                },
                api::switchboard_supervisor::JobGatewayEndpoint {
                    base_domain: "gw-eu-central-1.treadmillusercontent.com".to_string(),
                    port: 4433,
                },
            ],
        };

        let h = harness(virtual_size, virtual_size);
        let job_id = Uuid::new_v4();

        // Nothing is answered for a job this supervisor does not run.
        assert!(h.sup.gateway(host_id, job_id).await.is_none());

        start_and_boot(&h, start_msg_with_gateway(job_id, Some(dispatched.clone()))).await;

        let relayed = h
            .sup
            .gateway(host_id, job_id)
            .await
            .expect("a job dispatched with a gateway is told about it");
        assert_eq!(relayed.issuer, dispatched.issuer);
        assert_eq!(relayed.signing_public_key, dispatched.signing_public_key);
        assert_eq!(relayed.key_id, dispatched.key_id);
        assert_eq!(
            relayed.endpoints,
            dispatched
                .endpoints
                .iter()
                .cloned()
                .map(
                    |api::switchboard_supervisor::JobGatewayEndpoint { base_domain, port }| {
                        api::supervisor_puppet::JobGatewayEndpoint { base_domain, port }
                    }
                )
                .collect::<Vec<_>>()
        );

        // A job dispatched without one has none to be told about.
        let plain = harness(virtual_size, virtual_size);
        let plain_job = Uuid::new_v4();
        start_and_boot(&plain, start_msg(plain_job)).await;
        assert!(plain.sup.gateway(host_id, plain_job).await.is_none());
    }

    /// A service announcement is relayed to the coordinator as it arrives: the
    /// supervisor stores nothing and interprets nothing. An announcement
    /// carries a job's whole set, so a later one replaces the earlier rather
    /// than adding to it — including an empty one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_announced_service_set_is_relayed_to_the_coordinator() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);

        let job_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();
        start_and_boot(&h, start_msg(job_id)).await;
        assert!(h.connector.service_sets().is_empty());

        let announced = vec![
            JobService {
                name: "webide".to_string(),
                label: Some("Web IDE".to_string()),
                protocol: "webapp".to_string(),
            },
            JobService {
                name: "shell".to_string(),
                label: None,
                protocol: "sshws".to_string(),
            },
        ];
        h.sup
            .job_service_set(0, announced.clone(), host_id, job_id)
            .await;
        h.sup.job_service_set(1, Vec::new(), host_id, job_id).await;

        // Puppet events are ordered against the job's state changes, so a
        // terminate the job has acted on proves both were relayed first.
        terminate(&h.sup, job_id).await.unwrap();
        assert_eq!(h.connector.service_sets(), vec![announced, Vec::new()]);
    }

    /// A job that fails on its way up still owes the coordinator a terminal
    /// transition (D2.2): the reported error is the *cause* of the
    /// termination, never a substitute for it. Its record is then retained,
    /// occupying the slot, until it is removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_startup_failure_reports_the_error_and_then_terminated() {
        // The image head's virtual size exceeds the working-disk ceiling, so the
        // job fails as ImageInvalid before any workload is launched.
        let h = harness(8 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);

        let job_id = Uuid::new_v4();

        QemuSupervisor::start_job(&h.sup, start_msg(job_id))
            .await
            .unwrap();
        let mut facts = job_facts(&h.sup).await;
        wait_for(&mut facts, terminated).await;

        let errors = h.connector.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0].error_kind, connector::JobErrorKind::ImageInvalid),
            "{:?}",
            errors[0],
        );

        // Validation failed before launch, and the job never reached
        // Booting/Ready.
        assert!(h.launcher.spawned.lock().unwrap().is_empty());
        let labels = h.connector.labels();
        assert!(
            !labels
                .iter()
                .any(|l| l == "initializing/booting" || l == "ready"),
            "{labels:?}",
        );

        // It did reach Terminated, exactly once.
        assert_eq!(labels.last().map(String::as_str), Some("terminated"));
        assert_eq!(labels.iter().filter(|l| *l == "terminated").count(), 1);

        // The failed job is retained: it still holds the slot until removed.
        let error = QemuSupervisor::start_job(&h.sup, start_msg(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::MaxConcurrentJobs),
            "{error:?}",
        );

        remove(&h.sup, job_id).await.unwrap();
        assert!(h.sup.slot.lock().await.is_none());
    }

    /// D2.3/D2.4: the coordinator may repeat either command, or send one for a
    /// job this supervisor never heard of. A postcondition that already holds
    /// is not an error, and no repeat produces a second terminal transition.
    /// Only removing a job that is still executing is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminate_and_remove_are_idempotent() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);
        let job_id = Uuid::new_v4();

        // Nothing is known about this job, so both commands are satisfied.
        terminate(&h.sup, job_id).await.unwrap();
        remove(&h.sup, job_id).await.unwrap();

        let mut facts = start_and_boot(&h, start_msg(job_id)).await;

        // A live job must be terminated before it can be removed.
        let error = remove(&h.sup, job_id).await.unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::NotTerminated),
            "{error:?}",
        );

        terminate(&h.sup, job_id).await.unwrap();
        assert!(terminated(&facts.borrow_and_update()));

        terminate(&h.sup, job_id).await.unwrap();
        remove(&h.sup, job_id).await.unwrap();
        remove(&h.sup, job_id).await.unwrap();
        assert!(h.sup.slot.lock().await.is_none());

        let labels = h.connector.labels();
        assert_eq!(
            labels.iter().filter(|l| *l == "terminating").count(),
            1,
            "{labels:?}",
        );
        assert_eq!(
            labels.iter().filter(|l| *l == "terminated").count(),
            1,
            "{labels:?}",
        );
        assert!(
            h.connector.errors().is_empty(),
            "{:?}",
            h.connector.errors()
        );
    }

    /// D2.1: this supervisor runs a single job, which occupies its slot from
    /// `StartJob` until `RemoveJob` — a terminated-but-retained job refuses a
    /// new one just as a live one does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_job_is_refused_while_one_occupies_the_slot() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);
        let occupant = Uuid::new_v4();
        let next = Uuid::new_v4();

        start_and_boot(&h, start_msg(occupant)).await;

        let error = QemuSupervisor::start_job(&h.sup, start_msg(occupant))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::JobAlreadyExists),
            "{error:?}",
        );

        let error = QemuSupervisor::start_job(&h.sup, start_msg(next))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::AlreadyRunning),
            "{error:?}",
        );

        terminate(&h.sup, occupant).await.unwrap();

        let error = QemuSupervisor::start_job(&h.sup, start_msg(next))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::MaxConcurrentJobs),
            "{error:?}",
        );

        remove(&h.sup, occupant).await.unwrap();
        QemuSupervisor::start_job(&h.sup, start_msg(next))
            .await
            .unwrap();
    }

    /// The stop hook is the start hook's counterpart: it runs once per job that
    /// ran the start hook, and never for a job that failed before it. It is
    /// part of releasing the job's resources, which the retention window defers
    /// until the removal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stop_hook_runs_once_and_only_after_the_start_hook() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness_with_hooks(virtual_size, virtual_size);
        let job_id = Uuid::new_v4();

        start_and_boot(&h, start_msg(job_id)).await;
        assert_eq!(hook_runs(&h.tmp, "start"), 1);
        assert_eq!(hook_runs(&h.tmp, "stop"), 0);

        terminate(&h.sup, job_id).await.unwrap();
        assert_eq!(hook_runs(&h.tmp, "stop"), 0);

        remove(&h.sup, job_id).await.unwrap();
        assert_eq!(hook_runs(&h.tmp, "start"), 1);
        assert_eq!(hook_runs(&h.tmp, "stop"), 1);

        // A job failing before the start hook has nothing for the stop hook to
        // clean up after.
        let failed = harness_with_hooks(8 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);
        let failed_job = Uuid::new_v4();
        QemuSupervisor::start_job(&failed.sup, start_msg(failed_job))
            .await
            .unwrap();
        let mut failed_facts = job_facts(&failed.sup).await;
        wait_for(&mut failed_facts, terminated).await;

        remove(&failed.sup, failed_job).await.unwrap();
        assert_eq!(hook_runs(&failed.tmp, "start"), 0);
        assert_eq!(hook_runs(&failed.tmp, "stop"), 0);
    }

    /// Cancellation is structural, not polled: a stop that arrives while the
    /// job is inside an image fetch does not wait for that fetch to finish, and
    /// the fetch finishing afterwards does not run a second teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_terminated_mid_fetch_is_cancelled_where_it_stands() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let tmp = std::env::temp_dir().join(format!("tml-qemu-gate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let blob_file = tmp.join("root.qcow2");
        std::fs::write(&blob_file, b"not-a-real-qcow2").unwrap();

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let store = Arc::new(GatedStore {
            inner: StubStore {
                blob_file,
                manifest: single_layer_manifest(virtual_size),
            },
            entered: entered.clone(),
            release: release.clone(),
        });

        let h = harness_with_store(virtual_size, store);
        let job_id = Uuid::new_v4();

        QemuSupervisor::start_job(&h.sup, start_msg(job_id))
            .await
            .unwrap();
        let mut facts = job_facts(&h.sup).await;
        entered.notified().await;

        terminate(&h.sup, job_id).await.unwrap();
        assert!(terminated(&facts.borrow_and_update()));
        assert!(h.launcher.spawned.lock().unwrap().is_empty());

        // Releasing the fetch afterwards must not resurrect the job.
        release.notify_waiters();
        remove(&h.sup, job_id).await.unwrap();
        assert!(h.sup.slot.lock().await.is_none());

        let labels = h.connector.labels();
        assert_eq!(
            labels.iter().filter(|l| *l == "terminated").count(),
            1,
            "{labels:?}",
        );
        assert!(
            h.connector.errors().is_empty(),
            "{:?}",
            h.connector.errors()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
