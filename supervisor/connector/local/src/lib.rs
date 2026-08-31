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
//! The connector drives the supervisor through a [`connector::CoordCommand`]
//! channel, so it works with any supervisor that wires it in (the QEMU
//! supervisor today; the nbd-netboot supervisor once its job core lands). The
//! per-job inputs are parsed by the reusable [`LocalJobArgs`] (a
//! [`clap::Args`] each supervisor `main` can
//! `#[command(flatten)]`), keeping the supervisor protocol digest-addressed:
//! registry concerns (tag→digest resolution, pulling into the local store) are
//! the launcher's responsibility, not this connector's.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{Level, event};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageLocation, ImageSpecification, ParameterValue, RestartPolicy, RunningJobState,
    SupervisorEvent, SupervisorJobEvent,
};
use treadmill_rs::connector::{self, CoordCommand, JobError, StartJobMessage};
use treadmill_rs::image::Digest;

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
#[derive(Debug)]
pub struct LocalConnector {
    inner: Arc<Inner>,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug)]
struct Inner {
    /// Local OCI store authority (`host:port`) advertised as the image
    /// location; mirrors `[oci_store].registry` in the supervisor config.
    registry: String,
    args: LocalJobArgs,
    /// The job id this run drives (from `--job-id`, or freshly generated).
    job_id: Uuid,
    commands: mpsc::Sender<CoordCommand>,
    /// Observed by `run()`: set to `true` by `request_shutdown`.
    shutdown_rx: watch::Receiver<bool>,
    /// Set to `true` once the supervisor reports the job `Terminated`. `run()`
    /// waits on this to notice a job that ended on its own.
    terminated_tx: watch::Sender<bool>,
    terminated_rx: watch::Receiver<bool>,
}

impl LocalConnector {
    pub fn new(registry: String, args: LocalJobArgs, commands: mpsc::Sender<CoordCommand>) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (terminated_tx, terminated_rx) = watch::channel(false);
        let job_id = args.job_id.unwrap_or_else(Uuid::new_v4);
        Self {
            inner: Arc::new(Inner {
                registry,
                args,
                job_id,
                commands,
                shutdown_rx,
                terminated_tx,
                terminated_rx,
            }),
            shutdown_tx,
        }
    }
}

#[async_trait]
impl connector::SupervisorConnector for LocalConnector {
    async fn run(&self) -> Result<(), ()> {
        Inner::run(&self.inner).await
    }

    /// `run()` stops the job and returns. Ignoring the send error is fine — it
    /// only fails if `run()` already returned.
    fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    async fn emit(&self, supervisor_event: SupervisorEvent) {
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
                // The error is the cause of a termination, never a substitute
                // for it: the terminal transition still follows.
                event!(Level::ERROR, %job_id, ?error, "job error reported");
            }
            other => {
                event!(Level::DEBUG, %job_id, ?other, "ignoring supervisor event");
            }
        }
    }
}

impl Inner {
    async fn run(self: &Arc<Self>) -> Result<(), ()> {
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
            gateway: None,
            // A local run has no switchboard to describe the host.
            host_spec: None,
        };

        event!(
            Level::INFO,
            job_id = %self.job_id,
            %manifest_digest,
            repository,
            "starting one-shot local job",
        );
        // A start failure has no acknowledgement: the supervisor reports it as
        // a job error, which `emit` folds into `terminated_tx` below.
        if self
            .commands
            .send(CoordCommand::StartJob(start))
            .await
            .is_err()
        {
            event!(Level::ERROR, "supervisor is not accepting commands");
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

        // Both commands are acknowledged once the supervisor has carried them
        // out, so this does not return while qemu is still being killed or its
        // resources are still being released.
        let job_id = self.job_id;
        if let Err(e) = self
            .request(|ack| CoordCommand::TerminateJob { job_id, ack })
            .await
        {
            event!(Level::WARN, error = ?e, "terminating the job returned an error");
        }
        if let Err(e) = self
            .request(|ack| CoordCommand::RemoveJob { job_id, ack })
            .await
        {
            event!(Level::WARN, error = ?e, "removing the job returned an error");
        }
        Ok(())
    }

    /// Issue one acknowledged command and wait for the supervisor's answer. A
    /// supervisor that is gone can no longer be holding the job, so its silence
    /// satisfies the command.
    async fn request(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<(), JobError>>) -> CoordCommand,
    ) -> Result<(), JobError> {
        let (ack, acked) = oneshot::channel();
        if self.commands.send(command(ack)).await.is_err() {
            return Ok(());
        }
        acked.await.unwrap_or(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use treadmill_rs::api::switchboard_supervisor::{
        JobInitializingStage, ReportedSupervisorStatus,
    };
    use treadmill_rs::connector::SupervisorConnector;

    /// Stands in for the supervisor core: drains the command channel, records
    /// what it was asked to do, and reports the lifecycle back through the
    /// connector. With `terminate_on_start` it reports `Terminated` right away
    /// (a job that ends on its own); otherwise it reaches `Booting` and only
    /// terminates when asked to.
    async fn serve(
        connector: Arc<LocalConnector>,
        mut commands: mpsc::Receiver<CoordCommand>,
        terminate_on_start: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) {
        let mut held: Option<(Uuid, RunningJobState)> = None;

        while let Some(command) = commands.recv().await {
            match command {
                CoordCommand::StartJob(req) => {
                    calls.lock().unwrap().push("start");
                    let state = if terminate_on_start {
                        RunningJobState::Terminated
                    } else {
                        RunningJobState::Initializing {
                            stage: JobInitializingStage::Booting,
                        }
                    };
                    held = Some((req.job_id, state.clone()));
                    connector.update_job_state(req.job_id, state, None).await;
                }

                CoordCommand::TerminateJob { job_id, ack } => {
                    calls.lock().unwrap().push("terminate");
                    held = Some((job_id, RunningJobState::Terminated));
                    connector
                        .update_job_state(job_id, RunningJobState::Terminated, None)
                        .await;
                    let _ = ack.send(Ok(()));
                }

                CoordCommand::RemoveJob { job_id: _, ack } => {
                    calls.lock().unwrap().push("remove");
                    held = None;
                    let _ = ack.send(Ok(()));
                }

                CoordCommand::StatusRequest { reply } => {
                    let _ = reply.send(match held.clone() {
                        None => ReportedSupervisorStatus::Idle,
                        Some((job_id, job_state)) => {
                            ReportedSupervisorStatus::HoldingJob { job_id, job_state }
                        }
                    });
                }
            }
        }
    }

    /// A connector with a stub supervisor draining its commands, plus the log
    /// of the commands that supervisor received.
    fn build(terminate_on_start: bool) -> (Arc<LocalConnector>, Arc<Mutex<Vec<&'static str>>>) {
        let args = LocalJobArgs {
            manifest_digest: Some(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .parse()
                    .unwrap(),
            ),
            repository: Some("treadmill/stub".to_string()),
            parameters: vec![],
            stop_after: None,
            job_id: Some(Uuid::new_v4()),
        };

        let (command_tx, command_rx) = mpsc::channel(8);
        let connector = Arc::new(LocalConnector::new(
            "127.0.0.1:5000".to_string(),
            args,
            command_tx,
        ));
        let calls = Arc::new(Mutex::new(vec![]));

        tokio::spawn(serve(
            connector.clone(),
            command_rx,
            terminate_on_start,
            calls.clone(),
        ));

        (connector, calls)
    }

    async fn wait_for_calls(calls: &Mutex<Vec<&'static str>>, expected: &[&str]) {
        for _ in 0..200 {
            if calls.lock().unwrap().as_slice() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "calls {:?} never reached {expected:?}",
            calls.lock().unwrap()
        );
    }

    /// Ctrl-C path: the job is running, a shutdown request makes `run()` stop
    /// the job and return `Ok`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_stops_running_job() {
        let (connector, calls) = build(false);
        let run = {
            let connector = connector.clone();
            tokio::spawn(async move { connector.run().await })
        };

        // The job started and reached Booting; it is now running.
        wait_for_calls(&calls, &["start"]).await;

        connector.request_shutdown();
        assert_eq!(run.await.unwrap(), Ok(()));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &["start", "terminate", "remove"]
        );
    }

    /// Self-termination path: a job that ends on its own makes `run()` return
    /// without ever issuing a terminate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_termination_exits_without_stop() {
        let (connector, calls) = build(true);
        assert_eq!(connector.run().await, Ok(()));
        assert_eq!(calls.lock().unwrap().as_slice(), &["start"]);
    }
}
