//! A one-shot, switchboard-less coordinator connector.
//!
//! The [`crate::LocalConnector`] drives a supervisor through a single job from
//! locally-supplied inputs instead of a remote switchboard. It is the
//! counterpart to [`treadmill_ws_connector::WsConnector`] for local
//! development: point a supervisor at a local OCI store (e.g. a per-developer
//! Zot), hand it a resolved image digest and repository, and it boots one job,
//! reports its lifecycle to the terminal, and tears down on guest exit or
//! Ctrl-C. No Postgres, NATS, or switchboard is involved.
//!
//! The connector is generic over [`connector::Supervisor`], so it works with
//! any supervisor that wires it in (the QEMU supervisor today; the nbd-netboot
//! supervisor once its job core lands). The per-job inputs are parsed by the
//! reusable [`LocalJobArgs`] (a [`clap::Args`] each supervisor `main` can
//! `#[command(flatten)]`), keeping the supervisor protocol digest-addressed:
//! registry concerns (tag→digest resolution, pulling into the local store) are
//! the launcher's responsibility, not this connector's.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tracing::{Level, event};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageLocation, ImageSpecification, ParameterValue, RestartPolicy, RunningJobState,
    SupervisorEvent, SupervisorJobEvent,
};
use treadmill_rs::connector::{self, StartJobMessage, StopJobMessage};
use treadmill_rs::image::Digest;

/// How long to wait for the supervisor to report `Terminated` after a stop is
/// requested before giving up and letting `run()` return anyway.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// Per-job inputs for a standalone supervisor run, parsed on the command line.
///
/// Each supervisor binary `#[command(flatten)]`s this into its own argument
/// struct; the values here are synthesized into the single [`StartJobMessage`]
/// the [`LocalConnector`] dispatches. The image is identified by its
/// content-addressed manifest digest plus the repository it is present under in
/// the supervisor's local OCI store (the launcher resolves a human tag to this
/// digest before invoking the supervisor).
/// `manifest_digest` and `repository` are `Option` only so the whole group can
/// be flattened into a supervisor's args without forcing them on connectors
/// that don't use them (a `clap` `Option<Args>` group is optional only when its
/// fields are individually optional). They are required in practice: the
/// supervisor `main` validates their presence when the `local` connector is
/// selected, and [`LocalConnector::run`] refuses to start a job without them.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct LocalJobArgs {
    /// OCI manifest digest (`sha256:<hex>`) of the image to run, as present in
    /// the supervisor's local OCI store. Required for the `local` connector.
    #[arg(long, value_parser = parse_digest)]
    pub manifest_digest: Option<Digest>,

    /// Repository path the image is present under in the local OCI store, e.g.
    /// `treadmill/ubuntu-22.04`. Required for the `local` connector.
    #[arg(long)]
    pub repository: Option<String>,

    /// An SSH public key to deploy into the guest (repeatable).
    #[arg(long = "ssh-key", value_name = "KEY")]
    pub ssh_keys: Vec<String>,

    /// A job parameter as `key=value` (repeatable).
    #[arg(short = 'p', long = "param", value_name = "KEY=VALUE", value_parser = parse_param)]
    pub parameters: Vec<(String, String)>,

    /// Stop the job automatically after this duration (e.g. `5m`, `30s`).
    /// Without it the job runs until the guest exits or Ctrl-C.
    #[arg(long, value_parser = humantime::parse_duration)]
    pub stop_after: Option<Duration>,

    /// Job id to use. Defaults to a fresh random UUID.
    #[arg(long)]
    pub job_id: Option<Uuid>,
}

fn parse_digest(s: &str) -> Result<Digest, String> {
    s.parse::<Digest>().map_err(|e| e.to_string())
}

fn parse_param(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected KEY=VALUE, got {s:?}")),
    }
}

/// A switchboard-less connector that drives a supervisor through a single job.
///
/// Like [`treadmill_ws_connector::WsConnector`], the connector and the
/// supervisor hold references to each other, so the supervisor is held as a
/// [`Weak`] (broken cyclically with [`Arc::new_cyclic`] at construction).
#[derive(Debug)]
pub struct LocalConnector<S: connector::Supervisor> {
    inner: Arc<Inner<S>>,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug)]
struct Inner<S: connector::Supervisor> {
    /// Local OCI store authority (`host:port`) advertised as the image
    /// location; mirrors `[oci_store].registry` in the supervisor config.
    registry: String,
    args: LocalJobArgs,
    /// The job id this run drives (from `--job-id`, or freshly generated).
    job_id: Uuid,
    supervisor: Weak<S>,
    /// Observed by `run()`: set to `true` by `request_shutdown`.
    shutdown_rx: watch::Receiver<bool>,
    /// Set to `true` once the supervisor reports the job `Terminated` (or a
    /// fatal job error, which the supervisors emit just before tearing the job
    /// down). `run()` waits on this to complete the one-shot.
    terminated_tx: watch::Sender<bool>,
    terminated_rx: watch::Receiver<bool>,
}

impl<S: connector::Supervisor> LocalConnector<S> {
    pub fn new(registry: String, args: LocalJobArgs, supervisor: Weak<S>) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (terminated_tx, terminated_rx) = watch::channel(false);
        let job_id = args.job_id.unwrap_or_else(Uuid::new_v4);
        Self {
            inner: Arc::new(Inner {
                registry,
                args,
                job_id,
                supervisor,
                shutdown_rx,
                terminated_tx,
                terminated_rx,
            }),
            shutdown_tx,
        }
    }

    /// Request a graceful shutdown: `run()` stops the job and returns. Wired to
    /// a Ctrl-C / signal handler by the supervisor `main`. Ignoring the send
    /// error is fine — it only fails if `run()` already returned.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

#[async_trait]
impl<S: connector::Supervisor> connector::SupervisorConnector for LocalConnector<S> {
    async fn run(&self) -> Result<(), ()> {
        Inner::run(&self.inner).await
    }

    async fn update_event(&self, supervisor_event: SupervisorEvent) {
        let SupervisorEvent::JobEvent { job_id, event } = supervisor_event;
        match event {
            SupervisorJobEvent::StateTransition {
                new_state,
                status_message,
            } => {
                event!(
                    Level::INFO,
                    %job_id,
                    ?new_state,
                    ?status_message,
                    "job state transition",
                );
                if matches!(new_state, RunningJobState::Terminated) {
                    let _ = self.inner.terminated_tx.send(true);
                }
            }
            SupervisorJobEvent::Error { error } => {
                // The supervisors report an error and then tear the job down;
                // some failure paths (e.g. image fetch) never reach
                // `Terminated`. Treat any reported error as terminal so the
                // one-shot `run()` does not hang.
                event!(Level::ERROR, %job_id, ?error, "job error reported");
                let _ = self.inner.terminated_tx.send(true);
            }
            other => {
                event!(Level::DEBUG, %job_id, ?other, "ignoring supervisor event");
            }
        }
    }
}

impl<S: connector::Supervisor> Inner<S> {
    async fn run(self: &Arc<Self>) -> Result<(), ()> {
        let Some(supervisor) = self.supervisor.upgrade() else {
            event!(
                Level::ERROR,
                "supervisor dropped before run(); cannot start job"
            );
            return Err(());
        };

        // The image fields are `Option` for flattening (see [`LocalJobArgs`]),
        // but a job cannot start without them.
        let (Some(manifest_digest), Some(repository)) =
            (self.args.manifest_digest, self.args.repository.as_deref())
        else {
            event!(
                Level::ERROR,
                "the local connector requires --manifest-digest and --repository",
            );
            return Err(());
        };

        let start = StartJobMessage {
            job_id: self.job_id,
            image_spec: ImageSpecification::Image {
                manifest_digest,
                locations: vec![ImageLocation {
                    registry: self.registry.clone(),
                    repository: repository.to_string(),
                }],
            },
            ssh_keys: self.args.ssh_keys.clone(),
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            parameters: self
                .args
                .parameters
                .iter()
                .cloned()
                .map(|(k, v)| {
                    (
                        k,
                        ParameterValue {
                            value: v,
                            secret: false,
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
            // Local runs stream qemu's console straight to the terminal (the
            // supervisor inherits stdio when this is `None`); no NATS needed.
            log_streaming: None,
        };

        event!(
            Level::INFO,
            job_id = %self.job_id,
            %manifest_digest,
            repository,
            "starting one-shot local job",
        );
        if let Err(e) = connector::Supervisor::start_job(&supervisor, start).await {
            event!(Level::ERROR, error = ?e, "failed to start job");
            return Err(());
        }

        let mut shutdown_rx = self.shutdown_rx.clone();
        let mut terminated_rx = self.terminated_rx.clone();

        let stop_after = async {
            match self.args.stop_after {
                Some(d) => tokio::time::sleep(d).await,
                // Never fires: leaves the job running until terminated/Ctrl-C.
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            // The job ended on its own (guest shut down) or hit a fatal error.
            _ = terminated_rx.wait_for(|t| *t) => {
                event!(Level::INFO, "job terminated; exiting");
                return Ok(());
            }
            _ = shutdown_rx.wait_for(|s| *s) => {
                event!(Level::INFO, "shutdown requested; stopping job");
            }
            _ = stop_after => {
                event!(Level::INFO, "stop-after elapsed; stopping job");
            }
        }

        // Graceful stop, then wait (bounded) for the supervisor to confirm
        // teardown so we don't return while qemu is still being killed.
        if let Err(e) = connector::Supervisor::stop_job(
            &supervisor,
            StopJobMessage {
                job_id: self.job_id,
            },
        )
        .await
        {
            event!(Level::WARN, error = ?e, "stop_job returned an error");
        }
        if tokio::time::timeout(STOP_GRACE, terminated_rx.wait_for(|t| *t))
            .await
            .is_err()
        {
            event!(
                Level::WARN,
                "job did not report Terminated within the grace period; exiting anyway",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use treadmill_rs::api::switchboard_supervisor::JobInitializingStage;
    use treadmill_rs::connector::{JobError, Supervisor, SupervisorConnector};

    /// A stub supervisor that records the start/stop calls it receives and
    /// reports lifecycle events back through its connector. When
    /// `terminate_on_start` is set it reports `Terminated` immediately (a job
    /// that ends on its own); otherwise it reaches `Booting` and only reports
    /// `Terminated` in response to `stop_job`.
    #[derive(Debug)]
    struct StubSupervisor {
        connector: Arc<LocalConnector<StubSupervisor>>,
        terminate_on_start: bool,
        calls: Mutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl Supervisor for StubSupervisor {
        async fn start_job(this: &Arc<Self>, req: StartJobMessage) -> Result<(), JobError> {
            this.calls.lock().unwrap().push("start");
            if this.terminate_on_start {
                this.connector
                    .update_job_state(req.job_id, RunningJobState::Terminated, None)
                    .await;
            } else {
                this.connector
                    .update_job_state(
                        req.job_id,
                        RunningJobState::Initializing {
                            stage: JobInitializingStage::Booting,
                        },
                        None,
                    )
                    .await;
            }
            Ok(())
        }

        async fn stop_job(this: &Arc<Self>, req: StopJobMessage) -> Result<(), JobError> {
            this.calls.lock().unwrap().push("stop");
            this.connector
                .update_job_state(req.job_id, RunningJobState::Terminated, None)
                .await;
            Ok(())
        }
    }

    fn build(
        terminate_on_start: bool,
    ) -> (Arc<StubSupervisor>, Arc<LocalConnector<StubSupervisor>>) {
        let args = LocalJobArgs {
            manifest_digest: Some(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .parse()
                    .unwrap(),
            ),
            repository: Some("treadmill/stub".to_string()),
            ssh_keys: vec![],
            parameters: vec![],
            stop_after: None,
            job_id: Some(Uuid::new_v4()),
        };

        let mut connector_opt = None;
        let supervisor = {
            let connector_opt = &mut connector_opt;
            Arc::new_cyclic(move |weak| {
                let connector = Arc::new(LocalConnector::new(
                    "127.0.0.1:5000".to_string(),
                    args,
                    weak.clone(),
                ));
                *connector_opt = Some(connector.clone());
                StubSupervisor {
                    connector,
                    terminate_on_start,
                    calls: Mutex::new(vec![]),
                }
            })
        };
        (supervisor, connector_opt.take().unwrap())
    }

    async fn wait_for_calls(sup: &Arc<StubSupervisor>, expected: &[&str]) {
        for _ in 0..200 {
            if sup.calls.lock().unwrap().as_slice() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "calls {:?} never reached {expected:?}",
            sup.calls.lock().unwrap()
        );
    }

    /// Ctrl-C path: the job is running, a shutdown request makes `run()` stop
    /// the job and return `Ok`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_stops_running_job() {
        let (sup, connector) = build(false);
        let run = {
            let connector = connector.clone();
            tokio::spawn(async move { connector.run().await })
        };

        // The job started and reached Booting; it is now running.
        wait_for_calls(&sup, &["start"]).await;

        connector.request_shutdown();
        assert_eq!(run.await.unwrap(), Ok(()));
        assert_eq!(sup.calls.lock().unwrap().as_slice(), &["start", "stop"]);

        // Keep the supervisor alive until after run() upgraded its Weak.
        drop(sup);
    }

    /// Self-termination path: a job that ends on its own makes `run()` return
    /// without ever issuing a stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_termination_exits_without_stop() {
        let (sup, connector) = build(true);
        assert_eq!(connector.run().await, Ok(()));
        assert_eq!(sup.calls.lock().unwrap().as_slice(), &["start"]);
        drop(sup);
    }
}
