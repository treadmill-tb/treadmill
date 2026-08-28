//! The job lifecycle every supervisor runs, independent of what it boots.
//!
//! A [`JobRunner`] owns a single job slot and drains the coordinator's
//! [`CoordCommand`]s into it. One task owns the job that occupies the slot: it
//! brings the job up through its [`JobBackend`], supervises the workload,
//! makes the one terminal transition, retains the terminal record until it is
//! removed, and then releases everything the job took.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    JobGatewayDispatch, JobInitializingStage, JobService, LogChannel, ParameterValue,
    ReportedSupervisorStatus, RunningJobState,
};
use treadmill_rs::connector::{
    CoordCommand, JobError, JobErrorKind, StartJobMessage, SupervisorConnector,
};
use treadmill_rs::control_socket;

use treadmill_tcp_control_socket_server::TcpControlSocket;

use crate::capture::{self, SerialSocket};
use crate::launcher::WorkloadProcess;
use crate::publisher::{LogPublisher, LogPublisherConfig};

/// How long teardown waits for the log publisher to drain before giving up on
/// the chunks it is still holding.
const PUBLISHER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const JOB_MAILBOX_CAPACITY: usize = 8;

/// Variables describing a job, seeded by the runner and its backend, extended
/// by the start hook, and handed to the workload and the stop hook.
pub type JobVars = HashMap<String, String>;

/// What a supervisor needs to know to run a job, whatever it boots.
#[derive(Debug, Clone)]
pub struct JobRunnerConfig {
    pub supervisor_id: Uuid,

    /// Statically configured address of the host a job runs on, reported to
    /// the coordinator when set. Without it, the start hook may supply one as
    /// the `job_ip_address` variable.
    pub job_address: Option<IpAddr>,

    /// Directory the per-job working directories are created under.
    pub state_dir: PathBuf,

    /// Address the per-job puppet control socket listens on.
    pub control_socket_listen_addr: SocketAddr,

    pub start_script: Option<PathBuf>,
    pub stop_script: Option<PathBuf>,

    pub log_streaming: LogPublisherConfig,
}

/// The platform-specific half of a job: what it boots and how.
///
/// The runner drives these in order, reporting the matching phase before each
/// and running the start hook between [`allocate`](JobBackend::allocate) and
/// [`launch`](JobBackend::launch), so the hook can still influence the job
/// variables the workload is templated from.
#[async_trait]
pub trait JobBackend: std::fmt::Debug + Send + Sync + 'static {
    /// The image, resolved into whatever the backend needs to allocate from.
    type Image: Send;

    /// What the backend allocated for one job, consumed by `launch`.
    type Allocation: Send;

    /// Resolve the dispatched image specification.
    async fn fetch(&self, job: &StartJobMessage) -> Result<Self::Image, JobError>;

    /// Allocate what the job boots from, inside its working directory, and
    /// seed the variables the hooks and the workload see.
    async fn allocate(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        image: Self::Image,
        vars: &mut JobVars,
    ) -> Result<Self::Allocation, JobError>;

    /// Start the job's workload.
    async fn launch(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        allocation: Self::Allocation,
        vars: &JobVars,
    ) -> Result<Workload, JobError>;
}

/// A started workload, plus the console channels the runner ships to the
/// coordinator when the job was dispatched with log streaming.
pub struct Workload {
    pub process: Box<dyn WorkloadProcess>,

    /// The guest's serial console, when the backend routed it somewhere the
    /// runner can read.
    pub serial: Option<SerialSocket>,
}

/// Where a job is in its lifecycle, as the job's own task publishes it.
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
    pub fn running_job_state(&self) -> RunningJobState {
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

    pub fn terminated(&self) -> bool {
        matches!(self, Phase::Terminated { .. })
    }
}

/// Why a job stopped executing.
#[derive(Debug, Clone)]
pub enum Outcome {
    WorkloadExited(std::process::ExitStatus),
    TerminatedByRequest,
    CancelledDuringStartup,
    Failed(JobError),
}

impl Outcome {
    fn job_error(&self) -> Option<JobError> {
        match self {
            Outcome::Failed(error) => Some(error.clone()),
            Outcome::WorkloadExited(status) if !status.success() => Some(JobError {
                error_kind: JobErrorKind::InternalError,
                description: format!(
                    "Workload process had an internal error with status: {status:?}"
                ),
            }),
            _ => None,
        }
    }

    fn status_message(&self) -> String {
        match self {
            Outcome::WorkloadExited(status) if status.success() => {
                "Workload process exited successfully.".to_string()
            }
            Outcome::WorkloadExited(status) => {
                format!("Workload process had an internal error with status: {status:?}")
            }
            Outcome::TerminatedByRequest => "Workload process was killed.".to_string(),
            Outcome::CancelledDuringStartup => "Job terminated while starting up.".to_string(),
            Outcome::Failed(error) => error.description.clone(),
        }
    }
}

/// A lock-free snapshot of a job, published by its task and read by everyone
/// else.
#[derive(Debug, Clone)]
pub struct JobFacts {
    pub job_id: Uuid,
    pub phase: Phase,
    pub parameters: Arc<HashMap<String, ParameterValue>>,
    pub gateway: Arc<Option<JobGatewayDispatch>>,
    pub hostname: Arc<str>,
    pub network_address: Option<IpAddr>,
}

impl JobFacts {
    fn new(start_job_req: &StartJobMessage) -> Self {
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
        ack: oneshot::Sender<Result<(), JobError>>,
    },
    Remove {
        ack: oneshot::Sender<Result<(), JobError>>,
    },
    PuppetReady,
    PuppetTerminate,
    PuppetServiceSet(Vec<JobService>),
}

/// The only external reference to a running job: a command mailbox, a
/// lock-free facts snapshot, and a cancellation token.
#[derive(Debug, Clone)]
pub struct JobHandle {
    pub job_id: Uuid,
    cmd: mpsc::Sender<JobCommand>,
    facts: watch::Receiver<Arc<JobFacts>>,
    cancel: CancellationToken,
}

impl JobHandle {
    pub fn facts(&self) -> Arc<JobFacts> {
        self.facts.borrow().clone()
    }

    /// A receiver of this job's facts, which keeps serving the last published
    /// snapshot after the job's task is gone.
    pub fn facts_watch(&self) -> watch::Receiver<Arc<JobFacts>> {
        self.facts.clone()
    }

    async fn terminate(&self) -> Result<(), JobError> {
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

    async fn remove(&self) -> Result<(), JobError> {
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

/// Everything a job holds, owned by its task alone and freed in one place.
#[derive(Default)]
struct JobResources {
    control_socket: Option<TcpControlSocket<JobControlEndpoint>>,

    publisher: Option<LogPublisher>,

    workload: Option<Box<dyn WorkloadProcess>>,

    /// Variables associated with this job.
    ///
    /// Generated from default values in start job, can be modified or extended
    /// by the start script, later passed to the stop script.
    job_vars: JobVars,

    start_hook_ran: bool,
}

/// A supervisor's single job slot, and the loop that drives it.
#[derive(Debug)]
pub struct JobRunner<B: JobBackend> {
    connector: Arc<dyn SupervisorConnector>,
    backend: Arc<B>,
    config: JobRunnerConfig,

    /// The single job this supervisor runs, occupied from `StartJob` until
    /// `RemoveJob`.
    slot: Mutex<Option<JobSlot>>,
}

impl<B: JobBackend> JobRunner<B> {
    pub fn new(
        connector: Arc<dyn SupervisorConnector>,
        backend: Arc<B>,
        config: JobRunnerConfig,
    ) -> Self {
        JobRunner {
            connector,
            backend,
            config,
            slot: Mutex::new(None),
        }
    }

    /// Drain the coordinator's commands.
    ///
    /// The commands that change the job slot are inherently sequential, and
    /// each returns as soon as the job task has taken it on, so they are
    /// answered in the order they arrive rather than raced against each other.
    /// A status request only reads the slot, and is answered without waiting
    /// for them.
    pub async fn run(self: &Arc<Self>, mut commands: mpsc::Receiver<CoordCommand>) {
        while let Some(command) = commands.recv().await {
            match command {
                CoordCommand::StartJob(start_job_req) => {
                    let job_id = start_job_req.job_id;
                    if let Err(error) = self.start_job(start_job_req).await {
                        self.connector.report_job_error(job_id, error).await;
                    }
                }

                CoordCommand::TerminateJob { job_id, ack } => {
                    let _ = ack.send(self.terminate_job(job_id).await);
                }

                CoordCommand::RemoveJob { job_id, ack } => {
                    let _ = ack.send(self.remove_job(job_id).await);
                }

                CoordCommand::StatusRequest { reply } => {
                    let runner = self.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(runner.status().await);
                    });
                }
            }
        }
    }

    #[instrument(skip(self, start_job_req), fields(job_id = ?start_job_req.job_id), err(Debug, level = Level::WARN))]
    pub async fn start_job(
        self: &Arc<Self>,
        start_job_req: StartJobMessage,
    ) -> Result<(), JobError> {
        event!(Level::INFO, ?start_job_req);

        let mut slot_lg = self.slot.lock().await;

        if let Some(slot) = slot_lg.as_ref() {
            let facts = slot.handle.facts();
            return Err(if facts.job_id == start_job_req.job_id {
                JobError {
                    error_kind: JobErrorKind::JobAlreadyExists,
                    description: format!(
                        "Job {:?} already occupies this supervisor's job slot.",
                        facts.job_id,
                    ),
                }
            } else if facts.phase.terminated() {
                JobError {
                    error_kind: JobErrorKind::MaxConcurrentJobs,
                    description: format!(
                        "Supervisor {:?} still retains the terminated job {:?}, which has to be \
                         removed before another job can be started.",
                        self.config.supervisor_id, facts.job_id,
                    ),
                }
            } else {
                JobError {
                    error_kind: JobErrorKind::AlreadyRunning,
                    description: format!(
                        "Supervisor {:?} is already running job {:?}.",
                        self.config.supervisor_id, facts.job_id,
                    ),
                }
            });
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(JOB_MAILBOX_CAPACITY);
        let (facts_tx, facts_rx) = watch::channel(Arc::new(JobFacts::new(&start_job_req)));

        let handle = JobHandle {
            job_id: start_job_req.job_id,
            cmd: cmd_tx,
            facts: facts_rx,
            cancel: CancellationToken::new(),
        };

        let task = JobTask {
            runner: self.clone(),
            start_job_req,
            handle: handle.clone(),
            facts_tx,
            resources: JobResources::default(),
            terminate_acks: Vec::new(),
        };

        *slot_lg = Some(JobSlot {
            handle,
            task: tokio::spawn(task.run(cmd_rx)),
        });

        Ok(())
    }

    #[instrument(skip(self), err(Debug, level = Level::WARN))]
    pub async fn terminate_job(&self, job_id: Uuid) -> Result<(), JobError> {
        let Some(handle) = self.occupant(job_id).await else {
            return Ok(());
        };

        handle.terminate().await
    }

    #[instrument(skip(self), err(Debug, level = Level::WARN))]
    pub async fn remove_job(&self, job_id: Uuid) -> Result<(), JobError> {
        let Some(handle) = self.occupant(job_id).await else {
            return Ok(());
        };

        if !handle.facts().phase.terminated() {
            return Err(JobError {
                error_kind: JobErrorKind::NotTerminated,
                description: format!(
                    "Job {job_id:?} is still executing and must be terminated before removal.",
                ),
            });
        }

        handle.remove().await?;

        let slot = self.slot.lock().await.take();
        if let Some(slot) = slot {
            let _ = slot.task.await;
        }

        Ok(())
    }

    pub async fn status(&self) -> ReportedSupervisorStatus {
        match self.slot.lock().await.as_ref() {
            None => ReportedSupervisorStatus::Idle,
            Some(slot) => ReportedSupervisorStatus::HoldingJob {
                job_id: slot.handle.job_id,
                job_state: slot.handle.facts().phase.running_job_state(),
            },
        }
    }

    /// A handle to the job occupying the slot, whether it is still executing
    /// or a retained terminal record.
    pub async fn job(&self) -> Option<JobHandle> {
        self.slot.lock().await.as_ref().map(|s| s.handle.clone())
    }

    async fn occupant(&self, job_id: Uuid) -> Option<JobHandle> {
        match self.slot.lock().await.as_ref() {
            Some(slot) if slot.handle.job_id == job_id => Some(slot.handle.clone()),
            _ => None,
        }
    }

    async fn run_stop_job_script(&self, job_id: Uuid, job_vars: &JobVars) {
        if let Some(ref stop_script) = self.config.stop_script {
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
                        JobError {
                            error_kind: JobErrorKind::InternalError,
                            description,
                        },
                    )
                    .await;
            }
        }
    }
}

struct JobTask<B: JobBackend> {
    runner: Arc<JobRunner<B>>,
    start_job_req: StartJobMessage,
    handle: JobHandle,
    facts_tx: watch::Sender<Arc<JobFacts>>,
    resources: JobResources,
    terminate_acks: Vec<oneshot::Sender<Result<(), JobError>>>,
}

impl<B: JobBackend> JobTask<B> {
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
        self.runner
            .connector
            .update_job_state(self.job_id(), phase.running_job_state(), None)
            .await;
        self.update_facts(|facts| facts.phase = phase);
    }

    #[instrument(skip(self, cmd_rx), fields(job_id = ?self.job_id()))]
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<JobCommand>) {
        self.set_phase(Phase::Starting).await;

        let cancel = self.handle.cancel.clone();
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

    async fn startup(&mut self) -> Result<(), JobError> {
        self.set_phase(Phase::FetchingImage).await;
        let image = self.runner.backend.fetch(&self.start_job_req).await?;

        self.set_phase(Phase::Allocating).await;
        let job_workdir = allocate_workdir(&self.runner.config.state_dir, self.job_id()).await?;

        // Variables that can be produced by the start script, and used for
        // templating the workload's arguments or setting other job-specific
        // values (e.g., the host IP), populated with default values like the
        // Job ID and working directory.
        self.resources
            .job_vars
            .insert("job_id".to_string(), self.job_id().to_string());
        self.resources
            .job_vars
            .insert("job_workdir".to_string(), job_workdir.display().to_string());

        let allocation = self
            .runner
            .backend
            .allocate(
                &self.start_job_req,
                &job_workdir,
                image,
                &mut self.resources.job_vars,
            )
            .await?;

        self.run_start_job_script().await?;

        // Start a control socket for this job's puppet on the configured
        // listen addr, before the workload it belongs to comes up:
        let listen_addr = self.runner.config.control_socket_listen_addr;
        let control_socket = TcpControlSocket::new(
            self.runner.config.supervisor_id,
            self.job_id(),
            listen_addr,
            Arc::new(JobControlEndpoint {
                handle: self.handle.clone(),
            }),
        )
        .await
        .map_err(|e| JobError {
            error_kind: JobErrorKind::InternalError,
            description: format!("Failed to bind the control socket at {listen_addr:?}: {e:#}"),
        })?;
        self.resources.control_socket = Some(control_socket);

        let Workload {
            mut process,
            serial,
        } = self
            .runner
            .backend
            .launch(
                &self.start_job_req,
                &job_workdir,
                allocation,
                &self.resources.job_vars,
            )
            .await?;

        // Ship the captured console channels to NATS (durable spill + ack +
        // resume). Takes the stdout/stderr readers before the process is handed
        // to `supervise`. Spill files live under the per-job workdir so they
        // survive a supervisor restart and are retained for post-mortem after
        // the job ends.
        if let Some(dispatch) = self.start_job_req.log_streaming.clone() {
            let stdout = process.take_stdout();
            let stderr = process.take_stderr();
            let spill_dir = job_workdir.join("logs");
            let config = self.runner.config.log_streaming.clone();
            match LogPublisher::connect(&dispatch, spill_dir, config).await {
                Ok(publisher) => {
                    if let Some(stdout) = stdout {
                        publisher.spawn_channel(LogChannel::QemuStdout, stdout);
                    }
                    if let Some(stderr) = stderr {
                        publisher.spawn_channel(LogChannel::QemuStderr, stderr);
                    }
                    if let Some(socket) = serial {
                        publisher.spawn_serial(LogChannel::Serial, socket);
                    }
                    self.resources.publisher = Some(publisher);
                }
                Err(e) => {
                    // Don't fail the job over log-streaming setup; fall back to
                    // draining capture to our terminal so the workload's pipes
                    // don't block and the operator still sees output.
                    event!(
                        Level::WARN,
                        error = ?e,
                        "Failed to start log publisher; draining capture to terminal instead",
                    );
                    capture::drain_to_stdio(stdout, stderr, serial);
                }
            }
        }

        self.resources.workload = Some(process);

        // Booting, but puppet has not yet reported "ready":
        self.set_phase(Phase::Booting).await;

        self.report_job_address().await;

        Ok(())
    }

    async fn run_start_job_script(&mut self) -> Result<(), JobError> {
        let Some(start_script) = self.runner.config.start_script.clone() else {
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
        .map_err(|description| JobError {
            error_kind: JobErrorKind::InternalError,
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
        let mut job_address = self.runner.config.job_address;
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
        self.runner
            .connector
            .report_job_network_address(self.job_id(), job_address)
            .await;
    }

    async fn supervise(&mut self, cmd_rx: &mut mpsc::Receiver<JobCommand>) -> Outcome {
        let Some(mut workload) = self.resources.workload.take() else {
            return Outcome::Failed(JobError {
                error_kind: JobErrorKind::InternalError,
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
                    return Outcome::Failed(JobError {
                        error_kind: JobErrorKind::InternalError,
                        description: format!("Failed to wait on the workload process: {e:?}"),
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
                    let _ = ack.send(Err(JobError {
                        error_kind: JobErrorKind::NotTerminated,
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
                    self.runner
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
            event!(Level::WARN, error = ?e, "Failed to kill the workload process");
        }

        if let Some(error) = outcome.job_error() {
            self.runner
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
    ) -> Option<oneshot::Sender<Result<(), JobError>>> {
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
            self.runner
                .run_stop_job_script(self.job_id(), &job_vars)
                .await;
        }

        if let Some(publisher) = publisher {
            publisher.drain(PUBLISHER_DRAIN_TIMEOUT).await;
        }
    }
}

/// Create a job's working directory under the configured state dir. A
/// directory that is already there belonged to a job of the same id.
async fn allocate_workdir(state_dir: &Path, job_id: Uuid) -> Result<PathBuf, JobError> {
    let jobs_dir = state_dir.join("jobs");
    let job_dir = jobs_dir.join(job_id.to_string());

    event!(Level::DEBUG, ?job_dir, "Creating job state dir");

    tokio::fs::create_dir_all(&jobs_dir)
        .await
        .map_err(|io_err| JobError {
            error_kind: JobErrorKind::InternalError,
            description: format!("Failed to create state dir for job {job_id}: {io_err:?}"),
        })?;

    match tokio::fs::create_dir(&job_dir).await {
        Ok(()) => Ok(job_dir),

        Err(io_err) if io_err.kind() == std::io::ErrorKind::AlreadyExists => Err(JobError {
            error_kind: JobErrorKind::JobAlreadyExists,
            description: format!("A job with {job_id:?} was previously started on this supervisor",),
        }),

        Err(io_err) => Err(JobError {
            error_kind: JobErrorKind::InternalError,
            description: format!("Failed to create state dir for job {job_id}: {io_err:?}"),
        }),
    }
}

/// The puppet-facing view of one job, served by that job's control socket.
#[derive(Debug)]
pub struct JobControlEndpoint {
    handle: JobHandle,
}

impl JobControlEndpoint {
    pub fn new(handle: JobHandle) -> Self {
        JobControlEndpoint { handle }
    }

    fn handle(&self, tgt_job_id: Uuid) -> Option<&JobHandle> {
        if self.handle.job_id != tgt_job_id {
            event!(
                Level::WARN,
                ?tgt_job_id,
                "Received a puppet request for a job this endpoint does not serve",
            );
            return None;
        }
        Some(&self.handle)
    }

    fn facts(&self, tgt_job_id: Uuid) -> Option<Arc<JobFacts>> {
        let facts = self.handle(tgt_job_id)?.facts();
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
}

#[async_trait]
impl control_socket::Supervisor for JobControlEndpoint {
    #[instrument(skip(self))]
    async fn network_config(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::NetworkConfig> {
        let facts = self.facts(tgt_job_id)?;
        Some(treadmill_rs::api::supervisor_puppet::NetworkConfig {
            hostname: facts.hostname.to_string(),
            // Supervisors running a job under their own networking don't supply
            // a network interface to configure:
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
        let facts = self.facts(tgt_job_id)?;
        Some((*facts.parameters).clone())
    }

    #[instrument(skip(self))]
    async fn gateway(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::JobGatewayInfo> {
        let facts = self.facts(tgt_job_id)?;

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

        if let Some(handle) = self.handle(job_id) {
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
        // phase transitions to be well-defined, and governed by the supervisor,
        // not the host.
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

        if let Some(handle) = self.handle(job_id) {
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

        if let Some(handle) = self.handle(job_id) {
            handle.notify(JobCommand::PuppetServiceSet(services));
        }
    }
}

#[cfg(test)]
mod tests {
    //! In-process drive of the job lifecycle against a stub backend.
    //!
    //! With the backend and the connector behind traits, the runner's state
    //! machine can be driven from `StartJob` to `RemoveJob` — asserting the
    //! reported transitions, the slot's occupancy rules, and the teardown
    //! ordering — without spawning a single real binary.

    use super::*;

    use std::process::ExitStatus;

    use tempfile::TempDir;
    use tokio::sync::{Notify, oneshot};
    use uuid::Uuid;

    use treadmill_rs::api;
    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, ImageSpecification, JobGatewayDispatch, RestartPolicy, SupervisorEvent,
        SupervisorJobEvent,
    };
    use treadmill_rs::control_socket::Supervisor as _;

    const COMMAND_MAILBOX_CAPACITY: usize = 8;

    /// Connector that records the job state transitions and errors reported to
    /// it.
    #[derive(Debug, Default)]
    struct RecordingConnector {
        states: std::sync::Mutex<Vec<RunningJobState>>,
        errors: std::sync::Mutex<Vec<JobError>>,
        addresses: std::sync::Mutex<Vec<IpAddr>>,
        service_sets: std::sync::Mutex<Vec<Vec<JobService>>>,
    }

    impl RecordingConnector {
        fn labels(&self) -> Vec<String> {
            self.states.lock().unwrap().iter().map(label).collect()
        }

        fn errors(&self) -> Vec<JobError> {
            self.errors.lock().unwrap().clone()
        }

        fn addresses(&self) -> Vec<IpAddr> {
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

        async fn emit(&self, event: SupervisorEvent) {
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

    /// Holds a backend inside `fetch` until the test releases it.
    #[derive(Debug, Default)]
    struct Gate {
        entered: Notify,
        release: Notify,
    }

    /// A backend that allocates nothing and launches a workload that outlives
    /// its job, so what the runner does around it is all a test observes.
    #[derive(Debug, Default)]
    struct StubBackend {
        /// Fails `allocate` with this error rather than allocating.
        allocate_error: Option<JobError>,

        /// Blocks `fetch` until released, so a job can be stopped mid-fetch.
        fetch_gate: Option<Arc<Gate>>,

        launched: std::sync::Mutex<usize>,
    }

    impl StubBackend {
        fn failing(error_kind: JobErrorKind) -> Self {
            StubBackend {
                allocate_error: Some(JobError {
                    error_kind,
                    description: "the stub backend refuses to allocate".to_string(),
                }),
                ..StubBackend::default()
            }
        }

        fn gated(gate: Arc<Gate>) -> Self {
            StubBackend {
                fetch_gate: Some(gate),
                ..StubBackend::default()
            }
        }

        fn launched(&self) -> usize {
            *self.launched.lock().unwrap()
        }
    }

    #[async_trait]
    impl JobBackend for StubBackend {
        type Image = ();
        type Allocation = ();

        async fn fetch(&self, _job: &StartJobMessage) -> Result<(), JobError> {
            if let Some(gate) = &self.fetch_gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            Ok(())
        }

        async fn allocate(
            &self,
            _job: &StartJobMessage,
            _workdir: &Path,
            _image: (),
            _vars: &mut JobVars,
        ) -> Result<(), JobError> {
            match &self.allocate_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }

        async fn launch(
            &self,
            _job: &StartJobMessage,
            _workdir: &Path,
            _allocation: (),
            _vars: &JobVars,
        ) -> Result<Workload, JobError> {
            *self.launched.lock().unwrap() += 1;
            Ok(Workload {
                process: Box::new(StubProcess),
                serial: None,
            })
        }
    }

    type Runner = Arc<JobRunner<StubBackend>>;

    /// A constructed runner plus the stubs wired into it, over a temp dir.
    struct Harness {
        runner: Runner,
        connector: Arc<RecordingConnector>,
        backend: Arc<StubBackend>,
        tmp: TempDir,
    }

    fn harness(backend: StubBackend) -> Harness {
        harness_with(backend, |_, _| ())
    }

    /// Like [`harness`], letting a test settle the deployment-shaped parts of
    /// the configuration — a job address, the hooks — against the temp dir the
    /// runner is built over.
    fn harness_with(
        backend: StubBackend,
        tune: impl FnOnce(&Path, &mut JobRunnerConfig),
    ) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let connector = Arc::new(RecordingConnector::default());
        let backend = Arc::new(backend);

        // No job address: a deployment without a gateway has none.
        let mut config = JobRunnerConfig {
            supervisor_id: Uuid::new_v4(),
            job_address: None,
            state_dir: tmp.path().join("state"),
            control_socket_listen_addr: "127.0.0.1:0".parse().unwrap(),
            start_script: None,
            stop_script: None,
            log_streaming: LogPublisherConfig::default(),
        };
        tune(tmp.path(), &mut config);

        Harness {
            runner: Arc::new(JobRunner::new(connector.clone(), backend.clone(), config)),
            connector,
            backend,
            tmp,
        }
    }

    /// Point the configuration at start and stop scripts that each append a
    /// line to `<tmp>/<start|stop>-hook.log`, so a test can count how often
    /// they ran (see [`hook_runs`]).
    fn with_hooks(tmp: &Path, config: &mut JobRunnerConfig) {
        config.start_script = Some(write_hook(tmp, "start"));
        config.stop_script = Some(write_hook(tmp, "stop"));
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

    const STUB_DIGEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";

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
                manifest_digest: STUB_DIGEST.parse().unwrap(),
                locations: vec![ImageLocation {
                    registry: "127.0.0.1:0".to_string(),
                    repository: "treadmill/stub".to_string(),
                }],
            },
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            parameters: HashMap::new(),
            log_streaming: None,
            gateway,
        }
    }

    async fn endpoint(runner: &Runner) -> JobControlEndpoint {
        JobControlEndpoint::new(runner.job().await.expect("a job occupies the slot"))
    }

    async fn job_facts(runner: &Runner) -> watch::Receiver<Arc<JobFacts>> {
        runner
            .job()
            .await
            .expect("a job occupies the slot")
            .facts_watch()
    }

    async fn idle(runner: &Runner) -> bool {
        matches!(runner.status().await, ReportedSupervisorStatus::Idle)
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
        h.runner.start_job(msg).await.unwrap();

        let mut facts = job_facts(&h.runner).await;
        wait_for(&mut facts, booting).await;

        endpoint(&h.runner)
            .await
            .puppet_ready(0, Uuid::new_v4(), job_id)
            .await;
        wait_for(&mut facts, ready).await;

        facts
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_lifecycle_transitions() {
        let h = harness(StubBackend::default());

        let job_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();

        h.runner.start_job(start_msg(job_id)).await.unwrap();
        let mut facts = job_facts(&h.runner).await;
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
        assert_eq!(h.backend.launched(), 1);

        // Puppet reports ready → the job goes Ready.
        endpoint(&h.runner)
            .await
            .puppet_ready(0, host_id, job_id)
            .await;
        wait_for(&mut facts, ready).await;
        assert_eq!(
            h.connector.labels().last().map(String::as_str),
            Some("ready")
        );

        // Terminating kills the (stub) workload and reports the terminal
        // transition before it returns.
        h.runner.terminate_job(job_id).await.unwrap();

        let labels = h.connector.labels();
        assert!(labels.iter().any(|l| l == "terminating"), "{labels:?}");
        assert_eq!(labels.last().map(String::as_str), Some("terminated"));
        assert!(h.connector.errors().is_empty());

        // The record is retained until it is removed.
        assert!(terminated(&facts.borrow_and_update()));
        h.runner.remove_job(job_id).await.unwrap();
        assert!(idle(&h.runner).await);
    }

    /// A supervisor configured with a job address reports it as the job starts,
    /// so the coordinator has somewhere to point a gateway at before the job is
    /// up. One without stays silent, and the job is reachable from nowhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_configured_job_address_is_reported_at_start() {
        let address: IpAddr = "fd00::2".parse().unwrap();
        let h = harness_with(StubBackend::default(), |_, config| {
            config.job_address = Some(address)
        });

        assert!(h.connector.addresses().is_empty(), "nothing has started");

        start_and_boot(&h, start_msg(Uuid::new_v4())).await;
        assert_eq!(h.connector.addresses(), vec![address]);

        let unconfigured = harness(StubBackend::default());
        start_and_boot(&unconfigured, start_msg(Uuid::new_v4())).await;
        assert!(unconfigured.connector.addresses().is_empty());
    }

    /// The puppet asks its supervisor what to validate service tokens against,
    /// and gets back exactly what the coordinator dispatched the job with —
    /// nothing if the job was dispatched without a gateway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_job_is_told_its_gateway_material() {
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

        let h = harness(StubBackend::default());
        let job_id = Uuid::new_v4();

        start_and_boot(&h, start_msg_with_gateway(job_id, Some(dispatched.clone()))).await;
        let puppet = endpoint(&h.runner).await;

        // Nothing is answered for a job this endpoint does not serve.
        assert!(puppet.gateway(host_id, Uuid::new_v4()).await.is_none());

        let relayed = puppet
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
        let plain = harness(StubBackend::default());
        let plain_job = Uuid::new_v4();
        start_and_boot(&plain, start_msg(plain_job)).await;
        assert!(
            endpoint(&plain.runner)
                .await
                .gateway(host_id, plain_job)
                .await
                .is_none()
        );
    }

    /// A service announcement is relayed to the coordinator as it arrives: the
    /// supervisor stores nothing and interprets nothing. An announcement
    /// carries a job's whole set, so a later one replaces the earlier rather
    /// than adding to it — including an empty one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_announced_service_set_is_relayed_to_the_coordinator() {
        let h = harness(StubBackend::default());

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
        let puppet = endpoint(&h.runner).await;
        puppet
            .job_service_set(0, announced.clone(), host_id, job_id)
            .await;
        puppet.job_service_set(1, Vec::new(), host_id, job_id).await;

        // Puppet events are ordered against the job's state changes, so a
        // terminate the job has acted on proves both were relayed first.
        h.runner.terminate_job(job_id).await.unwrap();
        assert_eq!(h.connector.service_sets(), vec![announced, Vec::new()]);
    }

    /// A job that fails on its way up still owes the coordinator a terminal
    /// transition (D2.2): the reported error is the *cause* of the
    /// termination, never a substitute for it. Its record is then retained,
    /// occupying the slot, until it is removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_startup_failure_reports_the_error_and_then_terminated() {
        let h = harness(StubBackend::failing(JobErrorKind::ImageInvalid));

        let job_id = Uuid::new_v4();

        h.runner.start_job(start_msg(job_id)).await.unwrap();
        let mut facts = job_facts(&h.runner).await;
        wait_for(&mut facts, terminated).await;

        let errors = h.connector.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0].error_kind, JobErrorKind::ImageInvalid),
            "{:?}",
            errors[0],
        );

        // Allocation failed before launch, and the job never reached
        // Booting/Ready.
        assert_eq!(h.backend.launched(), 0);
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
        let error = h
            .runner
            .start_job(start_msg(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::MaxConcurrentJobs),
            "{error:?}",
        );

        h.runner.remove_job(job_id).await.unwrap();
        assert!(idle(&h.runner).await);
    }

    /// D2.3/D2.4: the coordinator may repeat either command, or send one for a
    /// job this supervisor never heard of. A postcondition that already holds
    /// is not an error, and no repeat produces a second terminal transition.
    /// Only removing a job that is still executing is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminate_and_remove_are_idempotent() {
        let h = harness(StubBackend::default());
        let job_id = Uuid::new_v4();

        // Nothing is known about this job, so both commands are satisfied.
        h.runner.terminate_job(job_id).await.unwrap();
        h.runner.remove_job(job_id).await.unwrap();

        let mut facts = start_and_boot(&h, start_msg(job_id)).await;

        // A live job must be terminated before it can be removed.
        let error = h.runner.remove_job(job_id).await.unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::NotTerminated),
            "{error:?}",
        );

        h.runner.terminate_job(job_id).await.unwrap();
        assert!(terminated(&facts.borrow_and_update()));

        h.runner.terminate_job(job_id).await.unwrap();
        h.runner.remove_job(job_id).await.unwrap();
        h.runner.remove_job(job_id).await.unwrap();
        assert!(idle(&h.runner).await);

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
        let h = harness(StubBackend::default());
        let occupant = Uuid::new_v4();
        let next = Uuid::new_v4();

        start_and_boot(&h, start_msg(occupant)).await;

        let error = h.runner.start_job(start_msg(occupant)).await.unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::JobAlreadyExists),
            "{error:?}",
        );

        let error = h.runner.start_job(start_msg(next)).await.unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::AlreadyRunning),
            "{error:?}",
        );

        h.runner.terminate_job(occupant).await.unwrap();

        let error = h.runner.start_job(start_msg(next)).await.unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::MaxConcurrentJobs),
            "{error:?}",
        );

        h.runner.remove_job(occupant).await.unwrap();
        h.runner.start_job(start_msg(next)).await.unwrap();
    }

    /// The stop hook is the start hook's counterpart: it runs once per job that
    /// ran the start hook, and never for a job that failed before it. It is
    /// part of releasing the job's resources, which the retention window defers
    /// until the removal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stop_hook_runs_once_and_only_after_the_start_hook() {
        let h = harness_with(StubBackend::default(), with_hooks);
        let job_id = Uuid::new_v4();

        start_and_boot(&h, start_msg(job_id)).await;
        assert_eq!(hook_runs(h.tmp.path(), "start"), 1);
        assert_eq!(hook_runs(h.tmp.path(), "stop"), 0);

        h.runner.terminate_job(job_id).await.unwrap();
        assert_eq!(hook_runs(h.tmp.path(), "stop"), 0);

        h.runner.remove_job(job_id).await.unwrap();
        assert_eq!(hook_runs(h.tmp.path(), "start"), 1);
        assert_eq!(hook_runs(h.tmp.path(), "stop"), 1);

        // A job failing before the start hook has nothing for the stop hook to
        // clean up after.
        let failed = harness_with(StubBackend::failing(JobErrorKind::ImageInvalid), with_hooks);
        let failed_job = Uuid::new_v4();
        failed
            .runner
            .start_job(start_msg(failed_job))
            .await
            .unwrap();
        let mut failed_facts = job_facts(&failed.runner).await;
        wait_for(&mut failed_facts, terminated).await;

        failed.runner.remove_job(failed_job).await.unwrap();
        assert_eq!(hook_runs(failed.tmp.path(), "start"), 0);
        assert_eq!(hook_runs(failed.tmp.path(), "stop"), 0);
    }

    /// Cancellation is structural, not polled: a stop that arrives while the
    /// job is inside an image fetch does not wait for that fetch to finish, and
    /// the fetch finishing afterwards does not run a second teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_terminated_mid_fetch_is_cancelled_where_it_stands() {
        let gate = Arc::new(Gate::default());
        let h = harness(StubBackend::gated(gate.clone()));
        let job_id = Uuid::new_v4();

        h.runner.start_job(start_msg(job_id)).await.unwrap();
        let mut facts = job_facts(&h.runner).await;
        gate.entered.notified().await;

        h.runner.terminate_job(job_id).await.unwrap();
        assert!(terminated(&facts.borrow_and_update()));
        assert_eq!(h.backend.launched(), 0);

        // Releasing the fetch afterwards must not resurrect the job.
        gate.release.notify_waiters();
        h.runner.remove_job(job_id).await.unwrap();
        assert!(idle(&h.runner).await);

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
    }

    /// A refused `StartJob` has no acknowledgement to fail: the command loop
    /// owes the coordinator a reported job error instead, or the refusal is
    /// never heard. The commands are answered in the order they arrive, so the
    /// acknowledged terminate behind them proves the refusal already happened.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_start_is_reported_as_a_job_error() {
        let h = harness(StubBackend::default());

        let (commands, command_rx) = mpsc::channel(COMMAND_MAILBOX_CAPACITY);
        let runner = h.runner.clone();
        tokio::spawn(async move { runner.run(command_rx).await });

        let occupant = Uuid::new_v4();
        let refused = Uuid::new_v4();
        commands
            .send(CoordCommand::StartJob(start_msg(occupant)))
            .await
            .unwrap();
        commands
            .send(CoordCommand::StartJob(start_msg(refused)))
            .await
            .unwrap();

        let (ack, acked) = oneshot::channel();
        commands
            .send(CoordCommand::TerminateJob {
                job_id: occupant,
                ack,
            })
            .await
            .unwrap();
        acked.await.unwrap().unwrap();

        let errors = h.connector.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0].error_kind, JobErrorKind::AlreadyRunning),
            "{:?}",
            errors[0],
        );
    }
}
