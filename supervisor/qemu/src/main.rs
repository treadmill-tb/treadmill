use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use clap::Parser;
use log::{info, warn};
use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use tml_tcp_control_socket_server::TcpControlSocket;

mod image_store_client;

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

pub struct QemuSupervisorJobRunningState {
    /// The qemu process handle:
    qemu_proc: tokio::process::Child,

    /// Control socket handle:
    control_socket: TcpControlSocket<QemuSupervisor>,
    // /// Set of rendezvous proxy connections:
    // ssh_rendezvous_proxies: Vec<rendezvous_proxy::RendezvousProxy>,
}

pub enum QemuSupervisorJobState {
    /// State to indicate that the job is starting.
    ///
    /// We use this to reserve a spot in the [`QemuSupervisor`]'s `jobs` map,
    /// such that we can release the global HashMap lock afterwards.
    Starting,

    // FetchingImageManifest {
    // 	job_config: sse::StartJobMessage,
    // },

    // FetchingImage {
    //     job_config: sse::StartJobMessage,
    // },
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
        args: QemuSupervisorArgs,
        config: QemuSupervisorConfig,
    ) -> Self {
        QemuSupervisor {
            connector,
            jobs: Mutex::new(HashMap::new()),
            args,
            config,
        }
    }
}

#[async_trait]
impl connector::Supervisor for QemuSupervisor {
    async fn start_job(
        this: &Arc<Self>,
        msg: connector::StartJobRequest,
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
                job_id: msg.job_id,
                request_id: Some(msg.request_id),
                error_kind: connector::JobErrorKind::AlreadyRunning,
                description: format!(
                    "Job {:?} is already running and cannot be started again.",
                    msg.job_id
                ),
            });
        }

        // Don't start more jobs than we're allowed to. Currently, the QEMU
        // supervisor only supports one job at a time (otherwise we'd need to
        // reason about IP address assignment from a pool, customizable
        // parameters for each instance, etc.).
        if jobs_lg.len() > 1 {
            return Err(connector::JobError {
                job_id: msg.job_id,
                request_id: Some(msg.request_id),
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
        jobs_lg.insert(msg.job_id, job.clone());

        // Release the global lock here:
        std::mem::drop(jobs_lg);
        //
        // ========== GLOBAL `jobs` HASHMAP LOCK RELEASED ======================

        // The job was inserted into the `jobs` HashMap and initialized as
        // `Starting`, let the coordinator know:
        this.connector
            .update_job_state(
                msg.job_id,
                connector::JobState::Starting {
                    // Generic starting stage. We don't fetch, allocate or provision any
                    // resources right now, so report a generic state instead:
                    stage: connector::JobStartingStage::Starting,
                    status_message: None,
                },
            )
            .await;

        // Make sure that we have access to the requested image (it's loaded
        // into our local image cache). We don't support fetching an image yet,
        // but we still check whether it exists, and otherwise return an error:

        // Start a TCP control socket on the specified listen addr:
        let control_socket = TcpControlSocket::new(
            msg.job_id,
            this.config.qemu.tcp_control_socket_listen_addr,
            this.clone(),
        )
        .await
        .unwrap();

        let qemu_proc = tokio::process::Command::new(&this.config.qemu.qemu_binary)
            .arg("--transport")
            .arg("auto_discover")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();

        // Job has been started, let the coordinator know:
        this.connector
            .update_job_state(
                msg.job_id,
                connector::JobState::Starting {
                    // Booting, but puppet has not yet reported "ready":
                    stage: connector::JobStartingStage::Booting,
                    status_message: None,
                },
            )
            .await;

        // Mark the job as started:
        *job_lg = QemuSupervisorJobState::Running(QemuSupervisorJobRunningState {
            control_socket,
            qemu_proc,
        });

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
                    job_id: msg.job_id,
                    request_id: Some(msg.request_id),
                    error_kind: connector::JobErrorKind::JobNotFound,
                    description: format!("Job {:?} not found, cannot stop.", msg.job_id),
                })?
        };

        let mut job_lg = job.lock().await;

        // Make sure the job is in a state in which we can stop it. If so, place
        // it into the `Stopping` state.
        let prev_job_state = match std::mem::replace(&mut *job_lg, QemuSupervisorJobState::Stopping)
        {
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

            QemuSupervisorJobState::Running(running_state) => {
                // Right now, only the Running state can be stopped, and
                // thus we can just return this type here, no need for any
                // additional wrapping:
                running_state
            }

            prev_state @ QemuSupervisorJobState::Stopping => {
                // Put back the previous state:
                *job_lg = prev_state;

                return Err(connector::JobError {
                    job_id: msg.job_id,
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

        // Right now, we only have the running state that can be returned above.
        let QemuSupervisorJobRunningState {
            control_socket,
            mut qemu_proc,
        } = prev_job_state;

        // TODO: kindly request the puppet to shut down. Here we simply force it
        // to quit (by using a SIGKILL). This is not nice.
        qemu_proc.kill().await.unwrap();

        // Shut down the control socket server:
        control_socket.shutdown().await.unwrap();

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
                    QemuSupervisor::new(connector, args, config)
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
