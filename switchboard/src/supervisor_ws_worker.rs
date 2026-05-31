use std::{ops::ControlFlow, pin::Pin};

use anyhow::{Context, Result, anyhow};
use axum::extract::ws;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::{Duration, Instant, Sleep, interval, sleep};
use treadmill_rs::api::switchboard_supervisor::{
    ReportedSupervisorStatus, StopJobMessage, SwitchboardToSupervisor, TaskExitStatus,
};
use uuid::Uuid;

use crate::sql;

/// Bounds for the WebSocket-like duplex stream the worker speaks to a
/// supervisor over.
///
/// In production this is [`axum::extract::ws::WebSocket`]. Tests can supply
/// any value that implements the same `Stream` + `Sink` shape over
/// [`axum::extract::ws::Message`] — see the `tests` module below for an
/// `mpsc`-backed implementation.
pub trait SupervisorSocket:
    Stream<Item = Result<ws::Message, axum::Error>>
    + Sink<ws::Message, Error = axum::Error>
    + Send
    + Unpin
    + 'static
{
}

impl<T> SupervisorSocket for T where
    T: Stream<Item = Result<ws::Message, axum::Error>>
        + Sink<ws::Message, Error = axum::Error>
        + Send
        + Unpin
        + 'static
{
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct SupervisorWSWorkerConfig {
    /// How often the switchboard PINGs the supervisor over the WebSocket.
    /// Parsed at config-load time from a human-readable duration string
    /// (e.g. `"2s"`, `"500ms"`); negative values are impossible by type.
    #[serde(with = "humantime_serde")]
    pub supervisor_ping_interval: Duration,
    /// Maximum time the switchboard will wait for a PONG from the supervisor
    /// before declaring the peer dead and terminating the worker. Parsed at
    /// config-load time from a human-readable duration string (e.g. `"10s"`).
    #[serde(with = "humantime_serde")]
    pub supervisor_pong_dead: Duration,
}

/// DB-side identity and configuration for one supervisor worker, deliberately
/// kept free of the socket.
///
/// The split exists for an auto-trait reason: the production socket (axum's
/// `WebSocket`) is `Send` but not `Sync`, so any `&self` async method on a
/// struct that *contains* the socket would hold a `&` to a non-`Sync` value
/// across its `.await`s — and `&T: Send` requires `T: Sync`. Since the worker's
/// future is `tokio::spawn`'d (and so must be `Send`), that would force every
/// such method to demand `S: Sync`, which `WebSocket` can't satisfy. Isolating
/// the DB context here means `with_txn` and friends borrow only `&WorkerCtx`
/// (which *is* `Sync`), so they stay ergonomic `&self` methods without
/// constraining the socket type.
struct WorkerCtx {
    pool: PgPool,
    supervisor_id: Uuid,
    worker_instance_id: u64,
    config: SupervisorWSWorkerConfig,
}

pub struct SupervisorWSWorker<S: SupervisorSocket> {
    ctx: WorkerCtx,
    socket: S,
}

/// Error type for [`SupervisorWSWorker`] operations.
///
/// `Stale` is a distinct, non-fatal variant: it signals that a newer worker
/// has taken over for this supervisor and the current worker should exit
/// gracefully. Any other failure flows through `Other` and is treated as a
/// real error. `From<anyhow::Error>` lets call sites propagate ordinary
/// errors with `?`; staleness flows up the same way and is distinguished
/// only at the top-level [`SupervisorWSWorker::run`] entry point.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(
        "supervisor worker is no longer current \
         (this worker_instance_id: {this_worker}, \
         current worker_instance_id in database: {current_worker})"
    )]
    Stale {
        this_worker: u64,
        current_worker: u64,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type WorkerResult<T = ()> = Result<T, WorkerError>;

impl WorkerCtx {
    async fn obtain_worker_instance_id(pool: &PgPool, supervisor_id: Uuid) -> Result<u64> {
        let db_worker_instance_id: i64 =
            sql::supervisor::increment_worker_instance_id(supervisor_id, pool)
                .await
                .with_context(|| {
                    format!(
                        "Obtaining new worker instance ID for supervisor {:?}",
                        supervisor_id
                    )
                })?;

        Ok(db_worker_instance_id.try_into().expect(
            "Database invariant violated: worker_instance_id must be zero or \
	     positive integer!",
        ))
    }

    /// Run `f` inside a transaction that holds an exclusive lock on this
    /// supervisor's row for the duration of the closure.
    ///
    /// The lock serializes all worker writes for this supervisor: a concurrent
    /// `increment_worker_instance_id` (issued by a takeover attempt from a new
    /// worker) will block until this transaction commits or rolls back. After
    /// taking the lock, this checks the supervisor's `worker_instance_id`; if
    /// it no longer matches, the closure is not executed and
    /// [`WorkerError::Stale`] is returned so the caller can exit gracefully via
    /// `?`.
    ///
    /// IMPORTANT: do not `.await` any non-DB work inside `f`. The row lock is
    /// held for the entire duration of the closure, so unrelated awaited I/O
    /// (network, channels, sleeps) will pin the lock and block any takeover
    /// attempt from a new worker.
    ///
    /// This is a `&self` method on [`WorkerCtx`] (not on `SupervisorWSWorker`)
    /// precisely because `WorkerCtx` holds no socket: see the type's doc comment
    /// for why borrowing the socket across these `.await`s would be a problem.
    async fn with_txn<R, F>(&self, f: F) -> WorkerResult<R>
    where
        F: AsyncFnOnce(&mut Transaction<'_, Postgres>) -> Result<R>,
    {
        let mut txn = self.pool.begin().await.with_context(|| {
            format!(
                "Beginning SupervisorWSWorker transaction for supervisor {} \
                 (worker_instance_id {})",
                self.supervisor_id, self.worker_instance_id,
            )
        })?;

        let current_worker_instance_id: u64 =
            sql::supervisor::lock_and_get_current_worker(self.supervisor_id, &mut txn)
                .await
                .with_context(|| {
                    format!(
                        "Locking and reading worker_instance_id for supervisor {}",
                        self.supervisor_id,
                    )
                })?
                .try_into()
                .expect(
                    "Database invariant violated: worker_instance_id must be zero \
                     or positive integer!",
                );

        if current_worker_instance_id != self.worker_instance_id {
            // `txn` is rolled back on drop. The closure body never ran.
            return Err(WorkerError::Stale {
                this_worker: self.worker_instance_id,
                current_worker: current_worker_instance_id,
            });
        }

        let out = f(&mut txn).await?;

        txn.commit().await.with_context(|| {
            format!(
                "Committing SupervisorWSWorker transaction for supervisor {} \
                 (worker_instance_id {})",
                self.supervisor_id, self.worker_instance_id,
            )
        })?;

        Ok(out)
    }
}

impl<S: SupervisorSocket> SupervisorWSWorker<S> {
    #[tracing::instrument(skip(pool, socket))]
    pub async fn run(
        pool: PgPool,
        supervisor_id: Uuid,
        socket: S,
        config: SupervisorWSWorkerConfig,
    ) {
        match Self::run_inner(pool, supervisor_id, socket, config).await {
            Ok(()) => {
                tracing::info!("SupervisorWSWorker::run terminated successfully.");
            }
            Err(WorkerError::Stale {
                this_worker,
                current_worker,
            }) => {
                tracing::info!(
                    this_worker,
                    current_worker,
                    "SupervisorWSWorker::run exiting: a newer worker has taken over."
                );
            }
            Err(WorkerError::Other(e)) => {
                tracing::error!("SupervisorWSWorker::run terminated with error: {e:?}");
            }
        }
    }

    pub async fn run_inner(
        pool: PgPool,
        supervisor_id: Uuid,
        socket: S,
        config: SupervisorWSWorkerConfig,
    ) -> WorkerResult<()> {
        // A supervisor has just opened a new WebSocket connection and
        // successfully authenticated. This might be because it's a new
        // supervisor, because it restarted, or because the connection was
        // interrupted.
        //
        // At any given time, a supervisor must have at most one switchboard
        // worker (this very method) responsible for managing its
        // state. However, in the case of network issues etc., it might be that
        // there is another task that believes to still be responsible for this
        // supervisor.
        //
        // To solve these issues, we store a monotonically increasing "worker
        // instance ID" counter per supervisor. Each new worker instance first
        // atomically increments and obtains a unique worker instance ID, and
        // then runs any database operation inside `with_txn`, which holds an
        // exclusive row lock on the supervisor record for the duration of the
        // transaction and verifies the worker is still current as its first
        // statement. The row lock serializes worker-vs-worker contention: a
        // newer worker's `increment_worker_instance_id` blocks until any
        // in-flight `with_txn` from the previous worker commits or rolls back.
        // If, on locking the row, the current worker observes a mismatched
        // ID, the transaction is rolled back and `WorkerError::Stale` is
        // returned, signalling the worker to exit gracefully.

        // This should never fail, as the supervisor has successfully
        // authenticated:
        let worker_instance_id = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;

        // Now, using this ID, construct the worker instance:
        let worker = SupervisorWSWorker {
            ctx: WorkerCtx {
                pool,
                supervisor_id,
                worker_instance_id,
                config,
            },
            socket,
        };

        worker.run_loop().await
    }

    /// Reconcile switchboard and supervisor state when a supervisor (re)connects.
    ///
    /// # Principle
    ///
    /// The **supervisor** is ground truth for what is *physically executing*; the
    /// **switchboard** is the source of truth for what is *assigned*
    /// (`supervisors.current_job`). On (re)connect the worker sends a
    /// `StatusRequest`, awaits the correlated `StatusResponse` (a timeout is
    /// treated as a dead peer), and passes the reported
    /// [`ReportedSupervisorStatus`] in as `reported`. This method then applies
    /// idempotent commands and `job_state`-guarded DB transitions to converge the
    /// two views.
    ///
    /// Let `J_sb = supervisors.current_job` (read from the DB under
    /// [`WorkerCtx::with_txn`]) and `J_sup` = the supervisor's reported
    /// `OngoingJob` id, if any. The five cases:
    ///
    /// | # | Switchboard (`J_sb`) | Supervisor reports | Resolution |
    /// |---|----------------------|--------------------|------------|
    /// | 1 | none   | `Idle`             | aligned; no action |
    /// | 2 | none   | `OngoingJob(J)`    | unassigned/zombie → `StopJob(J)`; do not adopt |
    /// | 3 | `J_sb` | `Idle`             | job lost → finalize `J_sb` as `supervisor_dropped_job`; honor `RestartPolicy` (may re-issue `StartJob`) |
    /// | 4 | `J_sb` | `OngoingJob(J_sb)` | adopt reported `job_state`: DB `job_state := reported`. A `Terminated` state finalizes `J_sb` (see below) |
    /// | 5 | `J_sb` | `OngoingJob(J_sup)`, `J_sup ≠ J_sb` | finalize `J_sb` as `supervisor_dropped_job` (+ `RestartPolicy`); `J_sup` is unassigned → `StopJob(J_sup)` |
    ///
    /// # Idempotence
    ///
    /// Every resolution is either an idempotent command (`StartJob`/`StopJob`
    /// carry the `job_id` and are no-ops when they don't apply to the current job
    /// state) or a `job_state`-guarded DB transition, so a reconnect that replays
    /// reconciliation converges to the same state. All DB writes go through
    /// [`WorkerCtx::with_txn`] so the takeover/staleness guard covers them; the
    /// drop-and-maybe-restart transition of cases 3 and 5 is
    /// `sql::job::finalize_dropped_and_maybe_restart`.
    ///
    /// The `Terminated` fold: in case 4 a `reported` state of
    /// `RunningJobState::Terminated` does not map to a live `job_state`; it
    /// instead finalizes `J_sb` with `termination_reason = workload_exited`
    /// (`sql::job::finalize_terminated`, **no** restart policy — a clean exit is
    /// not a failure). The task outcome is preserved as last declared via
    /// `apply_task_outcome`. The worker then sends `StopJob(J_sb)` to acknowledge the
    /// terminal report, which the supervisor uses to drop its in-memory retained
    /// record. See the `RunningJobState` Rustdoc in
    /// `treadmill_rs::api::switchboard_supervisor` for the retained-terminal
    /// contract.
    ///
    /// The `tests` submodule specifies each case above.
    #[allow(dead_code)] // wired into the (re)connect path in Phase 7
    async fn reconcile(&mut self, reported: ReportedSupervisorStatus) -> WorkerResult<()> {
        use treadmill_rs::api::switchboard_supervisor::RunningJobState;

        let supervisor_id = self.ctx.supervisor_id;
        let at = chrono::Utc::now();

        // All reads and writes run under the takeover/staleness guard. The
        // closure returns the id of a job the worker must `StopJob` *after* the
        // transaction commits: we never await the socket while the supervisor
        // row lock is held (see `with_txn`'s contract).
        let stop_zombie: Option<Uuid> = self
            .ctx
            .with_txn(async move |txn| {
                let j_sb = sql::supervisor::fetch_current_job(supervisor_id, txn).await?;

                let stop = match (j_sb, reported) {
                    // Case 1: aligned — nothing assigned, supervisor idle.
                    (None, ReportedSupervisorStatus::Idle) => None,

                    // Case 2: unassigned zombie — stop it, do not adopt.
                    (None, ReportedSupervisorStatus::OngoingJob { job_id, .. }) => Some(job_id),

                    // Case 3: assigned job was lost — finalize (honoring the
                    // restart policy) and release the assignment.
                    (Some(j_sb), ReportedSupervisorStatus::Idle) => {
                        sql::job::finalize_dropped_and_maybe_restart(j_sb, supervisor_id, at, txn)
                            .await?;
                        None
                    }

                    // Cases 4 & 5: supervisor reports an ongoing job.
                    (
                        Some(j_sb),
                        ReportedSupervisorStatus::OngoingJob {
                            job_id: j_sup,
                            job_state,
                        },
                    ) if j_sup == j_sb => {
                        // Case 4: same job — adopt the reported execution state.
                        match job_state {
                            RunningJobState::Initializing { stage } => {
                                sql::job::set_running_state(
                                    j_sb,
                                    sql::job::SqlJobState::Initializing,
                                    Some(stage.into()),
                                    at,
                                    txn,
                                )
                                .await?;
                                None
                            }
                            RunningJobState::Ready => {
                                sql::job::set_running_state(
                                    j_sb,
                                    sql::job::SqlJobState::Ready,
                                    None,
                                    at,
                                    txn,
                                )
                                .await?;
                                None
                            }
                            RunningJobState::Terminating => {
                                sql::job::set_running_state(
                                    j_sb,
                                    sql::job::SqlJobState::Terminating,
                                    None,
                                    at,
                                    txn,
                                )
                                .await?;
                                None
                            }
                            // The supervisor reports the assigned job has
                            // terminated. Finalize it as `workload_exited` (no
                            // restart — a clean exit is not a failure), then
                            // `StopJob(j_sb)` to ack, which releases the
                            // supervisor's retained terminal record. The task
                            // outcome is preserved as last declared out-of-band.
                            RunningJobState::Terminated => {
                                sql::job::finalize_terminated(j_sb, supervisor_id, at, txn).await?;
                                Some(j_sb)
                            }
                        }
                    }

                    // Case 5: assigned and reported job ids disagree. The
                    // assigned job is lost (finalize + restart policy); the
                    // reported job is an unassigned zombie to stop.
                    (Some(j_sb), ReportedSupervisorStatus::OngoingJob { job_id: j_sup, .. }) => {
                        sql::job::finalize_dropped_and_maybe_restart(j_sb, supervisor_id, at, txn)
                            .await?;
                        Some(j_sup)
                    }
                };

                Ok(stop)
            })
            .await?;

        if let Some(job_id) = stop_zombie {
            self.send_command(SwitchboardToSupervisor::StopJob(StopJobMessage { job_id }))
                .await?;
        }

        Ok(())
    }

    /// Record a task outcome the supervisor declared via
    /// [`SupervisorJobEvent::DeclareExitStatus`], out-of-band of reconciliation.
    ///
    /// The write only lands while the job is dispatched to this supervisor
    /// (`sql::job::set_task_outcome` is guarded on the assignment pointer);
    /// returns `false` if the job is not currently assigned here (e.g. already
    /// finalized or never dispatched), in which case the event is dropped.
    ///
    /// [`SupervisorJobEvent::DeclareExitStatus`]: treadmill_rs::api::switchboard_supervisor::SupervisorJobEvent::DeclareExitStatus
    #[allow(dead_code)] // wired into the event path in Phase 7
    async fn apply_task_outcome(
        &mut self,
        job_id: Uuid,
        outcome: TaskExitStatus,
        message: Option<String>,
    ) -> WorkerResult<bool> {
        let supervisor_id = self.ctx.supervisor_id;
        let applied = self
            .ctx
            .with_txn(async move |txn| {
                Ok(
                    sql::job::set_task_outcome(job_id, supervisor_id, outcome.into(), message, txn)
                        .await?,
                )
            })
            .await?;
        Ok(applied)
    }

    /// Serialize a [`SwitchboardToSupervisor`] command as JSON and send it over
    /// the socket as a Text frame (the protocol is JSON-over-Text; see the
    /// protocol module).
    #[allow(dead_code)] // only reached via `reconcile`, wired up in Phase 7
    async fn send_command(&mut self, command: SwitchboardToSupervisor) -> WorkerResult<()> {
        let json = serde_json::to_string(&command)
            .context("serializing SwitchboardToSupervisor command")?;
        self.socket
            .send(ws::Message::Text(json.into()))
            .await
            .context("sending command to supervisor over WebSocket")?;
        Ok(())
    }

    async fn handle_supervisor_msg(
        &mut self,
        msg: ws::Message,
        pong_timeout: &mut Pin<Box<Sleep>>,
    ) -> WorkerResult<ControlFlow<(), ()>> {
        match msg {
            ws::Message::Close(_close_frame) => {
                tracing::info!("SupervisorWSWorker: supervisor sent close message, terminating.");
                Ok(ControlFlow::Break(()))
            }

            ws::Message::Ping(payload) => {
                tracing::trace!(
                    "SupervisorWSWorker: received PING from supervisor ({} bytes)",
                    payload.len()
                );
                self.socket
                    .send(ws::Message::Pong(payload))
                    .await
                    .map_err(anyhow::Error::from)?;
                Ok(ControlFlow::Continue(()))
            }

            ws::Message::Binary(payload) => {
                tracing::warn!(
                    "SupervisorWSWorker: received unexpected binary WebSocket message ({} bytes), discarding...",
                    payload.len()
                );
                Ok(ControlFlow::Continue(()))
            }

            ws::Message::Text(payload) => {
                tracing::trace!(
                    "SupervisorWSWorker: received text WebSocket message ({} bytes)",
                    payload.len()
                );

                // TODO: implement!

                Ok(ControlFlow::Continue(()))
            }

            ws::Message::Pong(_) => {
                tracing::trace!("SupervisorWSWorker: received PONG from supervisor");
                pong_timeout
                    .as_mut()
                    .reset(Instant::now() + self.ctx.config.supervisor_pong_dead);
                Ok(ControlFlow::Continue(()))
            }
        }
    }

    async fn run_loop(mut self) -> WorkerResult<()> {
        // Keepalive PING message interval, for PING messages sent from the
        // switchboard to the supervisor:
        let mut ping_interval = interval(self.ctx.config.supervisor_ping_interval);

        // Timeout for waiting on PONG messages from the supervisor. Reset every
        // time we're getting a PONG response from the supervisor:
        let mut pong_timeout = Box::pin(sleep(self.ctx.config.supervisor_pong_dead));

        loop {
            tokio::select! {
                // Receive incoming messages from the supervisor via WebSocket:
                opt_msg = self.socket.next() => {
                    let Some(res_msg) = opt_msg else {
                        tracing::info!("SupervisorWSWorker socket closed, terminating.");
                        return Ok(());
                    };

                    let msg = res_msg.context("SupervisorWSWorker reading message from supervisor WebSocket")?;

                    if let ControlFlow::Break(()) = self.handle_supervisor_msg(msg, &mut pong_timeout).await? {
                        // We've been asked to terminate:
                        return Ok(());
                    }
                }

                // Periodically sending a WebSocket PING message to the
                // supervisor. This does double duty:
                // - it keeps the connection from being marked as idle and can
                //   help us recognize dead supervisor connections;
                // - we use it to check that there is not a newer worker serving
                //   this supervisor, in which case we terminate.
                _ = ping_interval.tick() => {
                    // Run a dummy transaction against the database, this will
                    // be sufficient to determine whether we're still the most
                    // current worker.
                    self.ctx.with_txn(async |_| Ok(())).await?;

                    // Still current, now send a PING:
                    self.socket.send(ws::Message::Ping((&[][..]).into())).await.context("SupervisorWSWorker sending ping to supervisor")?;
                }

                // Timeout for waiting on a PONG message from the supervisor. If
                // we haven't received one within the timeout and this fires, it
                // means we should consider the connection dead.
                _ = &mut pong_timeout => {
                    return Err(anyhow!(
                        "SupervisorWSWorker did not receive PONG from supervisor in {:?}, terminating.",
                        self.ctx.config.supervisor_pong_dead
                    ).into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! DB-backed unit tests for `SupervisorWSWorker`.
    //!
    //! Every test is marked `#[ignore]` because `#[sqlx::test]` needs a
    //! live Postgres reachable via `DATABASE_URL`. To run them, enter the
    //! ephemeral-Postgres devshell and pass `--run-ignored only` (nextest)
    //! or `-- --ignored` (cargo test):
    //!
    //!     nix develop '.#database'
    //!     cargo nextest run --run-ignored only -p treadmill-switchboard
    //!     # or
    //!     cargo test -p treadmill-switchboard -- --ignored
    //!
    //! CI runs these via the `nextest-db` Nix flake check.
    //!
    //! `run_loop` is still `todo!()`, so the only paths exercised here are
    //! the worker setup (`obtain_worker_instance_id`) and the row-locking
    //! takeover guard (`with_txn`).

    use super::*;
    use crate::auth::token::SecurityToken;
    use std::collections::BTreeSet;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};
    use treadmill_rs::api::switchboard_supervisor::{
        RunningJobState, SwitchboardToSupervisor, TaskExitStatus,
    };

    /// Build a `SupervisorWSWorkerConfig` with the given ping interval /
    /// dead-pong deadline, both expressed in milliseconds. Tests use short
    /// values so the suite stays fast without hard-coding magic durations
    /// everywhere.
    fn worker_config(ping_interval_ms: u64, pong_dead_ms: u64) -> SupervisorWSWorkerConfig {
        SupervisorWSWorkerConfig {
            supervisor_ping_interval: Duration::from_millis(ping_interval_ms),
            supervisor_pong_dead: Duration::from_millis(pong_dead_ms),
        }
    }

    /// Minimal `SupervisorSocket` impl used by tests that don't exercise the
    /// socket: poll_next is forever-pending, the sink discards everything.
    struct NoSocket;

    impl Stream for NoSocket {
        type Item = Result<ws::Message, axum::Error>;
        fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Sink<ws::Message> for NoSocket {
        type Error = axum::Error;
        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, _: ws::Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A scripted `SupervisorSocket`: the test pushes inbound items via the
    /// returned sender (the "to worker" half) and observes the worker's
    /// outgoing messages on the returned receiver (the "from worker" half).
    ///
    /// Closing the to-worker sender makes the worker see an end-of-stream;
    /// dropping the from-worker receiver makes the worker's writes fail.
    struct ScriptedSocket {
        inbound: tokio::sync::mpsc::UnboundedReceiver<Result<ws::Message, axum::Error>>,
        outbound: tokio::sync::mpsc::UnboundedSender<ws::Message>,
    }

    impl Stream for ScriptedSocket {
        type Item = Result<ws::Message, axum::Error>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.inbound.poll_recv(cx)
        }
    }

    impl Sink<ws::Message> for ScriptedSocket {
        type Error = axum::Error;
        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, item: ws::Message) -> Result<(), Self::Error> {
            self.outbound
                .send(item)
                .map_err(|_| axum::Error::new(std::io::Error::other("test socket outbound closed")))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn scripted_socket() -> (
        tokio::sync::mpsc::UnboundedSender<Result<ws::Message, axum::Error>>,
        tokio::sync::mpsc::UnboundedReceiver<ws::Message>,
        ScriptedSocket,
    ) {
        let (to_worker, inbound) = tokio::sync::mpsc::unbounded_channel();
        let (outbound, from_worker) = tokio::sync::mpsc::unbounded_channel();
        (to_worker, from_worker, ScriptedSocket { inbound, outbound })
    }

    async fn insert_supervisor(pool: &PgPool) -> anyhow::Result<Uuid> {
        let supervisor_id = Uuid::new_v4();
        sql::supervisor::insert(
            supervisor_id,
            format!("test-supervisor-{supervisor_id}"),
            SecurityToken::generate(),
            &BTreeSet::new(),
            Vec::new(),
            pool,
        )
        .await?;
        Ok(supervisor_id)
    }

    fn worker(
        pool: PgPool,
        supervisor_id: Uuid,
        worker_instance_id: u64,
        config: SupervisorWSWorkerConfig,
    ) -> SupervisorWSWorker<NoSocket> {
        SupervisorWSWorker {
            ctx: WorkerCtx {
                pool,
                supervisor_id,
                worker_instance_id,
                config,
            },
            socket: NoSocket,
        }
    }

    /// Like [`worker`], but over a [`ScriptedSocket`] so a test can observe the
    /// worker's outgoing commands on the returned receiver. Used by the
    /// reconciliation tests, which assert that `StopJob`/`StartJob` are emitted.
    fn scripted_worker(
        pool: PgPool,
        supervisor_id: Uuid,
        worker_instance_id: u64,
        config: SupervisorWSWorkerConfig,
    ) -> (
        tokio::sync::mpsc::UnboundedSender<Result<ws::Message, axum::Error>>,
        tokio::sync::mpsc::UnboundedReceiver<ws::Message>,
        SupervisorWSWorker<ScriptedSocket>,
    ) {
        let (to_worker, from_worker, socket) = scripted_socket();
        let worker = SupervisorWSWorker {
            ctx: WorkerCtx {
                pool,
                supervisor_id,
                worker_instance_id,
                config,
            },
            socket,
        };
        (to_worker, from_worker, worker)
    }

    /// Insert a bare user and return its id (jobs are enqueued by a token, which
    /// is owned by a user). `#[sqlx::test]` gives each test a fresh DB with no
    /// fixtures, so the reconciliation tests build their own user→token→job chain.
    async fn insert_user(pool: &PgPool) -> anyhow::Result<Uuid> {
        let user_id = Uuid::new_v4();
        sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user')")
            .bind(user_id)
            .execute(pool)
            .await?;
        sqlx::query("insert into tml_switchboard.users (subject_id, username) values ($1, $2)")
            .bind(user_id)
            .bind(format!("user-{user_id}"))
            .execute(pool)
            .await?;
        Ok(user_id)
    }

    /// Insert an API token owned by `user_id` and return its id.
    async fn insert_token(pool: &PgPool, user_id: Uuid) -> anyhow::Result<Uuid> {
        let token_id = Uuid::new_v4();
        sqlx::query(
            "insert into tml_switchboard.api_tokens \
             (token_id, token, user_id, canceled, created_at, expires_at) \
             values ($1, $2, $3, null, now(), now() + interval '1 day')",
        )
        .bind(token_id)
        .bind(vec![0u8; 128])
        .bind(user_id)
        .execute(pool)
        .await?;
        Ok(token_id)
    }

    /// Insert a job already bound to `supervisor_id` in the given `job_state`
    /// (e.g. `"scheduled"`, `"ready"`). `started_at` is set iff the state is one
    /// of the executing states, matching the `started_at_iso_executing` CHECK.
    /// `remaining_restarts` seeds the restart policy (cases 3/5 honor it).
    async fn insert_job(
        pool: &PgPool,
        token_id: Uuid,
        supervisor_id: Uuid,
        job_state: &str,
        remaining_restarts: i32,
    ) -> anyhow::Result<Uuid> {
        let job_id = Uuid::new_v4();
        sqlx::query(
            "insert into tml_switchboard.jobs \
             (job_id, resume_job_id, restart_job_id, image_id, ssh_keys, restart_policy, \
              enqueued_by_token_id, tag_config, job_timeout, job_state, initializing_stage, \
              queued_at, started_at, dispatched_on_supervisor_id, ssh_endpoints, \
              termination_reason, task_exit_status, exit_message, terminated_at, last_updated_at) \
             values \
             ($1, null, null, $2, '{}'::text[], row($3)::tml_switchboard.restart_policy, \
              $4, '', interval '1 hour', $5::tml_switchboard.job_state, null, \
              now(), \
              case when $5::tml_switchboard.job_state \
                        in ('initializing', 'ready', 'terminating') \
                   then now() else null end, \
              $6, null, \
              null, null, null, null, default)",
        )
        .bind(job_id)
        .bind(vec![0u8; 32])
        .bind(remaining_restarts)
        .bind(token_id)
        .bind(job_state)
        .bind(supervisor_id)
        .execute(pool)
        .await?;
        Ok(job_id)
    }

    /// Point `supervisor_id.current_job` at `job_id` (or clear it with `None`).
    async fn set_current_job(
        pool: &PgPool,
        supervisor_id: Uuid,
        job_id: Option<Uuid>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "update tml_switchboard.supervisors set current_job = $2 where supervisor_id = $1",
        )
        .bind(supervisor_id)
        .bind(job_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Read back `supervisors.current_job`.
    async fn current_job_of(pool: &PgPool, supervisor_id: Uuid) -> anyhow::Result<Option<Uuid>> {
        Ok(sqlx::query_scalar(
            "select current_job from tml_switchboard.supervisors where supervisor_id = $1",
        )
        .bind(supervisor_id)
        .fetch_one(pool)
        .await?)
    }

    /// Read back a job's `job_state` as its text representation.
    async fn job_state_of(pool: &PgPool, job_id: Uuid) -> anyhow::Result<String> {
        Ok(
            sqlx::query_scalar(
                "select job_state::text from tml_switchboard.jobs where job_id = $1",
            )
            .bind(job_id)
            .fetch_one(pool)
            .await?,
        )
    }

    /// Decode an outbound frame as a [`SwitchboardToSupervisor`] command. The
    /// protocol is JSON-over-Text (see the protocol module); anything else is a
    /// bug in the worker.
    fn decode_outbound(
        msg: ws::Message,
    ) -> treadmill_rs::api::switchboard_supervisor::SwitchboardToSupervisor {
        match msg {
            ws::Message::Text(text) => serde_json::from_str(&text)
                .expect("outbound command must be valid SwitchboardToSupervisor JSON"),
            other => panic!("expected an outbound Text command frame, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn obtain_worker_instance_id_starts_at_one_and_increments(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        for expected in 1u64..=3 {
            let got = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
            assert_eq!(got, expected);
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn with_txn_runs_closure_and_commits_when_current(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let worker_instance_id = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let worker = worker(
            pool.clone(),
            supervisor_id,
            worker_instance_id,
            worker_config(50, 250),
        );

        let renamed = "renamed-inside-txn".to_string();
        let returned = worker
            .ctx
            .with_txn(async |txn| {
                sqlx::query!(
                    r#"
                    UPDATE tml_switchboard.supervisors
                    SET name = $2
                    WHERE supervisor_id = $1
                    "#,
                    supervisor_id,
                    renamed,
                )
                .execute(&mut **txn)
                .await?;
                Ok(123u32)
            })
            .await
            .expect("with_txn should succeed for the current worker");
        assert_eq!(returned, 123);

        let observed: String = sqlx::query_scalar!(
            "SELECT name FROM tml_switchboard.supervisors WHERE supervisor_id = $1",
            supervisor_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(observed, renamed, "transaction should have committed");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn with_txn_returns_stale_when_superseded(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let our_id = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        // Simulate a takeover: a newer worker bumps the instance ID.
        let newer_id = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        assert_eq!(newer_id, our_id + 1);

        let worker = worker(pool.clone(), supervisor_id, our_id, worker_config(50, 250));
        let err = worker
            .ctx
            .with_txn(async |_txn| -> anyhow::Result<()> {
                panic!("closure must not run when worker is stale")
            })
            .await
            .expect_err("with_txn should reject a stale worker");
        match err {
            WorkerError::Stale {
                this_worker,
                current_worker,
            } => {
                assert_eq!(this_worker, our_id);
                assert_eq!(current_worker, newer_id);
            }
            WorkerError::Other(e) => panic!("expected Stale, got Other({e:?})"),
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn with_txn_rolls_back_on_closure_error(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let worker_instance_id = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let worker = worker(
            pool.clone(),
            supervisor_id,
            worker_instance_id,
            worker_config(50, 250),
        );

        let result: WorkerResult<()> = worker
            .ctx
            .with_txn(async |txn| {
                sqlx::query!(
                    "UPDATE tml_switchboard.supervisors SET name = $2 WHERE supervisor_id = $1",
                    supervisor_id,
                    "should-be-rolled-back",
                )
                .execute(&mut **txn)
                .await?;
                Err(anyhow::anyhow!("intentional failure"))
            })
            .await;
        assert!(
            matches!(result, Err(WorkerError::Other(_))),
            "closure error should surface as WorkerError::Other"
        );

        let observed: String = sqlx::query_scalar!(
            "SELECT name FROM tml_switchboard.supervisors WHERE supervisor_id = $1",
            supervisor_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_ne!(
            observed, "should-be-rolled-back",
            "transaction should have rolled back"
        );
        Ok(())
    }

    // -- run_loop termination ----------------------------------------------
    //
    // These tests pin down the basic shutdown contract: whatever else the
    // run loop does, it must return when the peer goes away — either via
    // an explicit Close frame or by the socket stream ending. They use the
    // `ScriptedSocket` helper above and wrap `run` in a `timeout` so a
    // bug that wedges the loop surfaces as a deadline miss rather than a
    // hung test.

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_returns_when_peer_sends_close(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Peer sends Close as its first (and only) frame.
        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("scripted inbound channel must be open");

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            SupervisorWSWorker::run(pool, supervisor_id, socket, worker_config(50, 250)),
        )
        .await
        .expect("worker should return promptly after peer Close");

        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_returns_when_socket_stream_ends(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Peer disappears without a Close frame — stream end.
        drop(to_worker);

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            SupervisorWSWorker::run(pool, supervisor_id, socket, worker_config(50, 250)),
        )
        .await
        .expect("worker should return promptly on socket stream end");

        Ok(())
    }

    // -- keepalive: switchboard PINGs the supervisor and watches for PONGs --
    //
    // Each side of the connection runs its own keepalive independently: the
    // switchboard worker emits PINGs every `supervisor_ping_interval` and
    // declares the supervisor dead (terminating the loop) if no PONG comes
    // back within `supervisor_pong_dead` of its last PING. The supervisor
    // side runs the symmetric loop in `connector/ws/src/lib.rs` with its own
    // local config; neither side configures the other.

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_emits_periodic_pings_to_supervisor(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, mut from_worker, socket) = scripted_socket();

        // 50ms ping cadence, 5s dead-pong deadline — the deadline is wide
        // enough that the dead-peer guard never fires during this test.
        let cfg = worker_config(50, 5_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        for n in 1..=3 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), from_worker.recv())
                .await
                .unwrap_or_else(|_| panic!("expected PING #{n} within 2s"))
                .expect("from_worker channel must remain open while worker runs");
            assert!(
                matches!(msg, ws::Message::Ping(_)),
                "expected Ping #{n}, got {msg:?}"
            );
        }

        // Tidy up: closing inbound ends the worker.
        drop(to_worker);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), jh).await;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_exits_when_no_pong_within_dead_threshold(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Tight deadline — worker should declare the peer dead within ~200ms.
        let cfg = worker_config(50, 200);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        // We never respond with Pongs; the worker must give up on its own.
        // Generous outer bound: pong-dead (200ms) + slack for scheduling/DB
        // setup. If the worker doesn't return here, the dead-peer detector
        // either isn't wired up or its deadline is wrong.
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker must terminate within pong-dead deadline when no Pong arrives")
            .expect("worker task should not panic");

        // Keep `to_worker` alive until the end so the worker's exit is
        // attributable to the dead-pong deadline, not stream-end.
        drop(to_worker);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_stays_alive_while_pongs_keep_arriving(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, mut from_worker, socket) = scripted_socket();

        // 50ms pings, 200ms dead-pong deadline: a missed Pong would kill the
        // worker within ~200ms. We'll run for ~600ms (3 dead-pong windows)
        // replying promptly each time, then close cleanly. The worker must
        // outlive all three windows.
        let cfg = worker_config(50, 200);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        for n in 1..=6 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), from_worker.recv())
                .await
                .unwrap_or_else(|_| panic!("expected PING #{n} within 2s"))
                .expect("from_worker channel must remain open while worker runs");
            let payload = match msg {
                ws::Message::Ping(p) => p,
                other => panic!("expected Ping #{n}, got {other:?}"),
            };
            to_worker
                .send(Ok(ws::Message::Pong(payload)))
                .expect("inbound channel must stay open");
        }

        // Worker should still be running — close it cleanly.
        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("inbound channel must stay open");
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker should return promptly after Close")
            .expect("worker task should not panic");
        Ok(())
    }

    // -- non-Close inbound frames must not panic the run-loop ----------------
    //
    // Currently `handle_supervisor_msg` is `_ => unimplemented!()` for every
    // variant other than Close. These tests pin down what each frame type
    // should do at the worker layer:
    //   * Ping → tolerated (production: tungstenite auto-pongs at the framing
    //            layer; the worker code itself only needs to not panic).
    //   * Pong → consumed by the dead-peer detector; loop continues.
    //   * Binary → log + continue (out-of-protocol).
    //   * Text → log + continue (full dispatch arrives with step 3).
    // In all four cases the worker must keep running until we send Close.

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn inbound_ping_does_not_terminate_loop(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Wide ping cadence / dead-pong deadline so neither fires during this
        // test — we only care that an inbound Ping doesn't take down the loop.
        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        // In production the Pong reply is emitted by the tungstenite framing
        // layer wrapped by axum's WebSocket; the application-level worker only
        // observes the Ping and must tolerate it. `ScriptedSocket` bypasses
        // tungstenite, so no auto-pong fires here — and we don't assert one.
        to_worker
            .send(Ok(ws::Message::Ping(b"ping-payload".as_slice().into())))
            .expect("inbound channel must stay open");

        // Now close cleanly and confirm the worker returns. If the previous
        // frame had panicked or short-circuited the loop, this Close would
        // never be observed and the timeout below would fire.
        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("inbound channel must stay open");
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker should still be running and return promptly after Close")
            .expect("worker task should not panic on inbound Ping");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn inbound_pong_does_not_terminate_loop(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        // Unsolicited Pong — must not panic the run loop.
        to_worker
            .send(Ok(ws::Message::Pong(b"unsolicited".as_slice().into())))
            .expect("inbound channel must stay open");

        // Now close cleanly and confirm the worker returns. If the previous
        // frame had panicked or short-circuited the loop, this Close would
        // never be observed and the timeout below would fire.
        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("inbound channel must stay open");
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker should still be running and return promptly after Close")
            .expect("worker task should not panic on inbound Pong");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn inbound_binary_does_not_terminate_loop(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        // The protocol is JSON-over-Text; Binary frames aren't part of it.
        // The loop should log and continue rather than panic.
        to_worker
            .send(Ok(ws::Message::Binary(b"\x00\x01\x02".as_slice().into())))
            .expect("inbound channel must stay open");

        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("inbound channel must stay open");
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker should still be running and return promptly after Close")
            .expect("worker task should not panic on inbound Binary");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn inbound_unparseable_text_does_not_terminate_loop(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, supervisor_id, socket, cfg));

        // Garbage that won't deserialize as a `switchboard_supervisor::Message`.
        // Until step 3 wires up real dispatch, the only contract we need is
        // "don't take down the loop."
        to_worker
            .send(Ok(ws::Message::Text("not a valid message".into())))
            .expect("inbound channel must stay open");

        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("inbound channel must stay open");
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker should still be running and return promptly after Close")
            .expect("worker task should not panic on unparseable Text");
        Ok(())
    }

    // -- reconciliation contract (Phase 5) ----------------------------------
    //
    // These pin down the five reconciliation cases documented on
    // `SupervisorWSWorker::reconcile`. They are RED until the Phase 7 worker
    // implements the logic (today `reconcile` is `todo!()`), so they fail by
    // panic rather than by assertion. Each test builds the DB precondition
    // (`J_sb = supervisors.current_job`), invokes `reconcile` with the reported
    // supervisor status (`J_sup`), and asserts the converged DB state plus any
    // command the worker must emit. `reconcile` is driven directly (it is not
    // yet wired into `run_loop`), mirroring how `with_txn` is unit-tested above.

    /// Case 1: switchboard has no assigned job and the supervisor is idle. The
    /// two views already agree, so reconcile is a no-op: nothing is assigned and
    /// no command is sent.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case1_idle_and_unassigned_is_noop(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        worker
            .reconcile(ReportedSupervisorStatus::Idle)
            .await
            .expect("case 1: reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, supervisor_id).await?,
            None,
            "case 1: no job should be assigned"
        );
        assert!(
            from_worker.try_recv().is_err(),
            "case 1: no command should be emitted"
        );
        Ok(())
    }

    /// Case 2: switchboard has no assigned job but the supervisor reports an
    /// ongoing one. That job is an unassigned zombie: the worker must `StopJob`
    /// it and must not adopt it.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case2_unassigned_zombie_is_stopped(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        let zombie = Uuid::new_v4();
        worker
            .reconcile(ReportedSupervisorStatus::OngoingJob {
                job_id: zombie,
                job_state: RunningJobState::Ready,
            })
            .await
            .expect("case 2: reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, supervisor_id).await?,
            None,
            "case 2: zombie must not be adopted"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("case 2: a StopJob command should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, zombie),
            other => panic!("case 2: expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// Case 3: switchboard has an assigned job but the supervisor reports idle —
    /// the job was lost. The worker finalizes it as `supervisor_dropped_job` and
    /// clears the assignment. (Restart policy is 0 here, so no successor.)
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case3_lost_job_is_finalized(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, supervisor_id, "ready", 0).await?;
        set_current_job(&pool, supervisor_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        worker
            .reconcile(ReportedSupervisorStatus::Idle)
            .await
            .expect("case 3: reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "case 3: lost job must be finalized"
        );
        assert_eq!(
            current_job_of(&pool, supervisor_id).await?,
            None,
            "case 3: assignment must be cleared"
        );
        Ok(())
    }

    /// Case 4: switchboard and supervisor agree on the job id. The worker adopts
    /// the supervisor's reported execution state into the DB `job_state` and
    /// keeps the assignment.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case4_same_job_adopts_reported_state(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        // Bound but not yet started; the supervisor reports it is now running.
        let job_id = insert_job(&pool, token_id, supervisor_id, "scheduled", 0).await?;
        set_current_job(&pool, supervisor_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        worker
            .reconcile(ReportedSupervisorStatus::OngoingJob {
                job_id,
                job_state: RunningJobState::Ready,
            })
            .await
            .expect("case 4: reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "ready",
            "case 4: DB must adopt the reported execution state"
        );
        assert_eq!(
            current_job_of(&pool, supervisor_id).await?,
            Some(job_id),
            "case 4: assignment must be retained"
        );
        Ok(())
    }

    /// Read a job's `task_exit_status` (as its enum text) and `exit_message`.
    async fn task_outcome_of(
        pool: &PgPool,
        job_id: Uuid,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "select task_exit_status::text, exit_message \
             from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?;
        Ok(row)
    }

    /// Case 4, `Terminated`: the supervisor reports the assigned job has
    /// terminated. The switchboard finalizes it (with `workload_exited`, not
    /// `supervisor_dropped_job`), clears the assignment, and `StopJob`s the job
    /// to acknowledge the terminal report. The task outcome the supervisor
    /// declared out-of-band before termination must be preserved across the
    /// finalize.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case4_terminated_finalizes_and_acks(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, supervisor_id, "ready", 0).await?;
        set_current_job(&pool, supervisor_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        // The supervisor declares the outcome out-of-band, before reporting the
        // terminated state. It must survive finalization untouched.
        assert!(
            worker
                .apply_task_outcome(
                    job_id,
                    TaskExitStatus::Success,
                    Some("workload exited cleanly".to_string()),
                )
                .await
                .expect("declaring the outcome should succeed"),
            "the outcome must apply while the job is assigned"
        );

        worker
            .reconcile(ReportedSupervisorStatus::OngoingJob {
                job_id,
                job_state: RunningJobState::Terminated,
            })
            .await
            .expect("case 4 (terminated): reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "case 4 (terminated): the job must be finalized"
        );
        let reason: Option<String> = sqlx::query_scalar(
            "select termination_reason::text from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            reason.as_deref(),
            Some("workload_exited"),
            "case 4 (terminated): must finalize as workload_exited, not supervisor_dropped_job"
        );
        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (
                Some("success".to_string()),
                Some("workload exited cleanly".to_string())
            ),
            "case 4 (terminated): the declared outcome must be preserved across finalize"
        );
        let job_count: i64 = sqlx::query_scalar("select count(*) from tml_switchboard.jobs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            job_count, 1,
            "case 4 (terminated): a clean exit must not enqueue a restart successor"
        );
        assert_eq!(
            current_job_of(&pool, supervisor_id).await?,
            None,
            "case 4 (terminated): assignment must be cleared"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("case 4 (terminated): a StopJob ack should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("case 4 (terminated): expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// A supervisor may declare the task outcome while it is assigned the job,
    /// independently of the job's lifecycle state.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn apply_task_outcome_while_assigned_persists(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, supervisor_id, "ready", 0).await?;
        set_current_job(&pool, supervisor_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        assert!(
            worker
                .apply_task_outcome(job_id, TaskExitStatus::Pending, Some("booting".to_string()))
                .await?,
            "setting the outcome on an assigned job must succeed"
        );
        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (Some("pending".to_string()), Some("booting".to_string())),
        );
        Ok(())
    }

    /// The outcome may be set repeatedly, each call overriding the last value,
    /// and the message may be revised or cleared (passing `None`).
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn apply_task_outcome_overrides_and_clears_message(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, supervisor_id, "ready", 0).await?;
        set_current_job(&pool, supervisor_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        worker
            .apply_task_outcome(job_id, TaskExitStatus::Pending, Some("running".to_string()))
            .await?;
        // Override pending -> success.
        worker
            .apply_task_outcome(job_id, TaskExitStatus::Success, Some("done".to_string()))
            .await?;
        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (Some("success".to_string()), Some("done".to_string())),
            "the latest outcome and message must win"
        );
        // A later call may clear the message while keeping an outcome set.
        worker
            .apply_task_outcome(job_id, TaskExitStatus::Failure, None)
            .await?;
        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (Some("failure".to_string()), None),
            "passing None must clear the message; the outcome stays set"
        );
        Ok(())
    }

    /// Declaring an outcome for a job that is dispatched to a *different*
    /// supervisor is a no-op (returns `false`) and writes nothing: the setter is
    /// guarded on this supervisor's assignment pointer.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn apply_task_outcome_not_assigned_is_noop(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let other_supervisor = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        // The job is dispatched to `other_supervisor`, not to our worker.
        let job_id = insert_job(&pool, token_id, other_supervisor, "ready", 0).await?;
        set_current_job(&pool, other_supervisor, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        assert!(
            !worker
                .apply_task_outcome(job_id, TaskExitStatus::Success, Some("nope".to_string()))
                .await?,
            "setting the outcome on a job assigned elsewhere must be a no-op"
        );
        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (None, None),
            "no outcome must be written for a job assigned to another supervisor"
        );
        Ok(())
    }

    /// Case 5: switchboard and supervisor disagree on the job id. The assigned
    /// job is lost (finalize as `supervisor_dropped_job`, clear assignment) while
    /// the reported job is an unassigned zombie that must be `StopJob`'d.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case5_mismatched_job_finalizes_and_stops(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let assigned = insert_job(&pool, token_id, supervisor_id, "ready", 0).await?;
        set_current_job(&pool, supervisor_id, Some(assigned)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), supervisor_id, wiid, worker_config(50, 250));

        let reported = Uuid::new_v4();
        worker
            .reconcile(ReportedSupervisorStatus::OngoingJob {
                job_id: reported,
                job_state: RunningJobState::Ready,
            })
            .await
            .expect("case 5: reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, assigned).await?,
            "finalized",
            "case 5: the assigned-but-lost job must be finalized"
        );
        assert_eq!(
            current_job_of(&pool, supervisor_id).await?,
            None,
            "case 5: assignment must be cleared"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("case 5: a StopJob command for the reported job should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, reported),
            other => panic!("case 5: expected StopJob, got {other:?}"),
        }
        Ok(())
    }
}
