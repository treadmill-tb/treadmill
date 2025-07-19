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

use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

// TODO: to port!
// use treadmill_sse_connector::SSEConnector;

use treadmill_rs::api::switchboard_supervisor::{SupervisorEvent, SupervisorJobEvent};
use treadmill_tcp_control_socket_server::TcpControlSocket;

#[derive(Parser, Debug, Clone)]
pub struct MockSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,

    /// Path to the puppet binary to run as the main process of a job
    #[arg(short, long)]
    puppet_binary: PathBuf,
}

// #[derive(Deserialize, Debug, Clone)]
// #[serde(rename_all = "snake_case")]
// pub enum SSHPreferredIPVersion {
//     Unspecified,
//     V4,
//     V6,
// }
//
// impl Default for SSHPreferredIPVersion {
//     fn default() -> Self {
//         SSHPreferredIPVersion::Unspecified
//     }
// }

#[derive(Deserialize, Debug, Clone)]
pub struct MockConfig {
    /// The number of parallel jobs we allow to be scheduled on this supervisor.
    max_parallel_jobs: usize,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MockSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configurations for individual connector implementations. All are
    /// optional, and not all of them have to be supported:
    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    mock: MockConfig,
}

#[derive(Debug)]
pub enum ControlSocket {
    Tcp(TcpControlSocket<MockSupervisor>),
}

#[derive(Debug)]
pub struct MockSupervisorJobRunningState {
    start_job_req: connector::StartJobMessage,

    /// The puppet process handle:
    puppet_proc: tokio::process::Child,

    /// Control socket handle:
    control_socket: ControlSocket,
}

#[derive(Debug)]
pub enum MockSupervisorJobState {
    /// State to indicate that the job is starting.
    ///
    /// We use this to reserve a spot in the [`MockSupervisor`]'s `jobs` map,
    /// such that we can release the global HashMap lock afterwards.
    Starting,

    // FetchingImageManifest {
    // 	job_config: sse::StartJobMessage,
    // },

    // FetchingImage {
    //     job_config: sse::StartJobMessage,
    // },
    /// State to indicate that the job is running.
    Running(MockSupervisorJobRunningState),

    /// State to indicate that the job is currently shutting down.
    ///
    /// While the job is in this state, no job with the same ID must be started
    /// / resumed. We might still be cleaning up resources associated with this
    /// job.
    Stopping,
}

impl MockSupervisorJobState {
    fn state_name(&self) -> &'static str {
        match self {
            MockSupervisorJobState::Starting => "Starting",
            MockSupervisorJobState::Running(_) => "Running",
            MockSupervisorJobState::Stopping => "Stopping",
        }
    }
}

#[derive(Debug)]
pub struct MockSupervisor {
    /// Connector to the switchboard. All communication is mediated through
    /// this connector.
    connector: Arc<dyn connector::SupervisorConnector>,

    /// We support running multiple jobs on one supervisor (in particular when
    /// not sharing hardware resources), so use a map of `Arc`s behind a mutex
    /// to avoid locking the map across long-running calls.
    jobs: Mutex<HashMap<Uuid, Arc<Mutex<MockSupervisorJobState>>>>,
    args: MockSupervisorArgs,
    config: MockSupervisorConfig,
}

impl MockSupervisor {
    pub fn new(
        connector: Arc<dyn connector::SupervisorConnector>,
        args: MockSupervisorArgs,
        config: MockSupervisorConfig,
    ) -> Self {
        MockSupervisor {
            connector,
            jobs: Mutex::new(HashMap::new()),
            args,
            config,
        }
    }

    async fn stop_job_internal(
        &self,
        job_id: Uuid,
        job: Arc<Mutex<MockSupervisorJobState>>,
    ) -> Result<(), connector::JobError> {
        // We do not immediately remove the job from the global jobs HashMap, as
        // we want to deallocate all resources before a job with an identical ID
        // can be resumed again. Thus, first transition it into a `Stopping`
        // state and return a reference to it. We take ownership of the old job
        // state and destruct it.

        let mut job_lg = job.lock().await;

        // Make sure the job is in a state in which we can stop it. If so, place
        // it into the `Stopping` state.
        let prev_job_state = match std::mem::replace(&mut *job_lg, MockSupervisorJobState::Stopping)
        {
            prev_state @ MockSupervisorJobState::Starting => {
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

            MockSupervisorJobState::Running(running_state) => {
                // Right now, only the Running state can be stopped, and
                // thus we can just return this type here, no need for any
                // additional wrapping:
                running_state
            }

            prev_state @ MockSupervisorJobState::Stopping => {
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
            .update_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: connector::RunningJobState::Terminating,
                    status_message: None,
                },
            })
            .await;

        // Right now, we only have the running state that can be returned above.
        let MockSupervisorJobRunningState {
            start_job_req: _,
            control_socket,
            mut puppet_proc,
        } = prev_job_state;

        // TODO: kindly request the puppet to shut down. Here we simply force it
        // to quit (by using a SIGKILL). This is not nice.
        puppet_proc.kill().await.unwrap();

        // Shut down the control socket server:
        match control_socket {
            ControlSocket::Tcp(cs) => cs.shutdown().await.unwrap(),
        }

        // Job has been stopped, let the coordinator know:
        self.connector
            .update_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: connector::RunningJobState::Terminated,
                    status_message: None,
                },
            })
            .await;

        // Finally, remove the job from the jobs HashMap. Eventually, all other
        // `Arc` references (including the one we hold) will get dropped.
        assert!(self.jobs.lock().await.remove(&job_id).is_some());

        Ok(())
    }
}

#[async_trait]
impl connector::Supervisor for MockSupervisor {
    async fn start_job(
        this: &Arc<Self>,
        msg: connector::StartJobMessage,
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
        if jobs_lg.get(&msg.job_id).is_some() {
            return Err(connector::JobError {
                error_kind: connector::JobErrorKind::AlreadyRunning,
                description: format!(
                    "Job {:?} is already running and cannot be started again.",
                    msg.job_id
                ),
            });
        }

        // Don't start more jobs than we're allowed to:
        if jobs_lg.len() > this.config.mock.max_parallel_jobs {
            return Err(connector::JobError {
                error_kind: connector::JobErrorKind::MaxConcurrentJobs,
                description: format!(
                    "Supervisor {:?} cannot start any more concurrent jobs (running {}, max {}).",
                    this.config.base.supervisor_id,
                    jobs_lg.len(),
                    this.config.mock.max_parallel_jobs
                ),
            });
        }

        // We're good to create this job, create it in the `Starting` state:
        let job = Arc::new(Mutex::new(MockSupervisorJobState::Starting));

        // Acquire a lock on the job. No one else has a reference yet, so this
        // should succeed immediately:
        let mut job_lg = job.lock().await;

        // Insert a clone of the Arc into the HashMap:
        jobs_lg.insert(msg.job_id, job.clone());

        // Release the global lock here:
        std::mem::drop(jobs_lg);
        //
        // ========== GLOBAL `jobs` HASHMAP LOCK RELEASED ======================

        // The job was inserted into the `jobs` HashMap and initialized as
        // `Starting`, let the coordinator know:
        this.connector
            .update_event(SupervisorEvent::JobEvent {
                job_id: msg.job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: connector::RunningJobState::Initializing {
                        // Generic starting stage. We don't fetch, allocate or provision any
                        // resources right now, so report a generic state instead:
                        stage: connector::JobInitializingStage::Starting,
                    },
                    status_message: None,
                },
            })
            .await;

        // Start a control socket server on TCP port 20202
        const SOCKET_ADDR_STR: &str = "[::1]:20202";
        let socket_addr =
            <std::net::SocketAddr as std::str::FromStr>::from_str(SOCKET_ADDR_STR).unwrap();

        let control_socket = TcpControlSocket::new(
            this.config.base.supervisor_id,
            msg.job_id,
            socket_addr,
            this.clone(),
        )
        .await
        .unwrap();

        let puppet_proc = tokio::process::Command::new(
            // Unfortunately, cargo bindeps is still a nightly feature and
            // break things, such as cargo fmt:
            //env!("CARGO_BIN_FILE_TML_PUPPET_tml-puppet")
            &this.args.puppet_binary,
        )
        .arg("daemon")
        .arg("--transport")
        .arg("tcp")
        .arg("--tcp-control-socket-addr")
        .arg(SOCKET_ADDR_STR)
        .arg("--dbus-bus")
        .arg("session")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
        // .env("TML_CTRLSOCK_UNIXSEQPACKET", &control_socket_path)

        // Job has been started, let the coordinator know:
        this.connector
            .update_event(SupervisorEvent::JobEvent {
                job_id: msg.job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: connector::RunningJobState::Initializing {
                        // Booting, but puppet has not yet reported "ready":
                        stage: connector::JobInitializingStage::Booting,
                    },
                    status_message: None,
                },
            })
            .await;

        // Mark the job as started:
        *job_lg = MockSupervisorJobState::Running(MockSupervisorJobRunningState {
            control_socket: ControlSocket::Tcp(control_socket),
            start_job_req: msg,
            puppet_proc,
        });

        Ok(())
    }

    async fn stop_job(
        this: &Arc<Self>,
        msg: connector::StopJobMessage,
    ) -> Result<(), connector::JobError> {
        // Get a reference to this job by an ephemeral lock on `jobs` HashMap:
        let job: Arc<Mutex<MockSupervisorJobState>> = {
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

        this.stop_job_internal(msg.job_id, job).await
    }
    //
    // async fn request_status(_this: &Arc<Self>) -> SupervisorStatus {
    //     // MC: I genuinely have no idea how this should be implemented
    //     todo!()
    // }
}

#[async_trait]
impl control_socket::Supervisor for MockSupervisor {
    async fn ssh_keys(&self, _host_id: Uuid, tgt_job_id: Uuid) -> Option<Vec<String>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // We don't actually store any SSH keys for the MockSupervisor
                // job, so just return an empty set:
                MockSupervisorJobState::Running(_) => Some(vec![]),

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
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::supervisor_puppet::NetworkConfig> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running, respond with its assigned hostname:
                MockSupervisorJobState::Running(_) => {
                    let hostname = format!("job-{}", format!("{tgt_job_id}").split_at(10).0);
                    Some(treadmill_rs::api::supervisor_puppet::NetworkConfig {
                        hostname,
                        // MockSupervisor, don't supply a network interface to configure:
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
                    "Received puppet network config request for non-existent job: {:?}",
                    tgt_job_id
                );
                None
            }
        }
    }

    async fn parameters(
        &self,
        _host_id: Uuid,
        tgt_job_id: Uuid,
    ) -> Option<HashMap<String, treadmill_rs::api::supervisor_puppet::ParameterValue>> {
        match self.jobs.lock().await.get(&tgt_job_id) {
            Some(job_state) => match &*job_state.lock().await {
                // Job is currently running:
                MockSupervisorJobState::Running(MockSupervisorJobRunningState {
                    start_job_req,
                    ..
                }) => Some(start_job_req.parameters.clone()),

                // Only respond to host / puppet requests when the job is marked
                // as "running":
                state => {
                    warn!(
                        "Received puppet parameters request for job {:?} in invalid state: {}",
                        tgt_job_id,
                        state.state_name()
                    );
                    None
                }
            },

            // Job not found:
            None => {
                warn!(
                    "Received puppet parameters request for non-existant job: {:?}",
                    tgt_job_id
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
                MockSupervisorJobState::Running(_) => {
                    self.connector
                        .update_event(SupervisorEvent::JobEvent {
                            job_id,
                            event: SupervisorJobEvent::StateTransition {
                                // TODO: connection info
                                new_state: connector::RunningJobState::Ready,
                                status_message: None,
                            },
                        })
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
        _host_id: Uuid,
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
        _host_id: Uuid,
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

    #[instrument(skip(self))]
    async fn terminate_job(
        &self,
        _puppet_event_id: u64,
        _supervisor_event_id: Option<u64>,
        _host_id: Uuid,
        job_id: Uuid,
    ) {
        event!(Level::INFO, "Received puppet event to terminate job",);

        // Get a reference to this job by an ephemeral lock on `jobs` HashMap:
        let job = if let Some(job) = self.jobs.lock().await.get(&job_id).cloned() {
            job
        } else {
            event!(
                Level::WARN,
                "Received puppet terminate job request for non-existant job {:?}",
                job_id,
            );
            return;
        };

        if let Err(e) = self.stop_job_internal(job_id, job).await {
            event!(Level::WARN, "Failed to stop job: {:?}", e);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    use treadmill_rs::connector::SupervisorConnector;

    tracing_subscriber::fmt::init();

    info!("Treadmill Mock Supervisor, Hello World!");

    let args = MockSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: MockSupervisorConfig = toml::from_str(&config_str).unwrap();

    match config.base.coord_connector {
        SupervisorCoordConnector::WsConnector => {
            let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                "Requested WsConnector, but `ws_connector` config not present."
            ))?;

            // Both the supervisor and connectors have references to each other,
            // so we break the cyclic dependency with an initially unoccupied
            // weak Arc reference:
            let mut connector_opt = None;

            let mock_supervisor = {
                // Shadow, to avoid moving the variable:
                let connector_opt = &mut connector_opt;
                Arc::new_cyclic(move |weak_supervisor| {
                    let connector = Arc::new(treadmill_ws_connector::WsConnector::new(
                        config.base.supervisor_id,
                        ws_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());
                    MockSupervisor::new(connector, args, config)
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

            std::mem::drop(mock_supervisor);

            Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
}
