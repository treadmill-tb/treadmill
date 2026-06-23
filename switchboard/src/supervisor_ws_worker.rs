use std::pin::Pin;

use anyhow::{Context, Result, anyhow};
use axum::extract::ws;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::{Duration, Instant, Sleep, interval, sleep};
use treadmill_rs::api::switchboard_supervisor::{
    ReportedSupervisorStatus, Request, RunningJobState, StopJobMessage, SupervisorEvent,
    SupervisorJobEvent, SupervisorToSwitchboard, SwitchboardToSupervisor, TaskExitStatus,
};
use uuid::Uuid;

use crate::log_streaming::LogStreaming;
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
    /// How often the worker runs a [`reconcile`] pass, converging the DB's
    /// desired state against the cached supervisor status. Deliberately separate
    /// from `supervisor_ping_interval`: a reconcile is also the dispatch trigger
    /// for newly-scheduled jobs, so its cadence sets scheduling latency, whereas
    /// the ping cadence is tuned for liveness detection. The dedicated timer is
    /// reset whenever a reconcile runs for another reason (a liveness tick, an
    /// inbound status response), so it only fires to cover quiet stretches. A
    /// reconcile never issues a `StatusRequest` itself — it reads the in-memory
    /// status cache that the ping-driven `StatusRequest` and (later) the event
    /// stream keep fresh — so a future Postgres pub/sub trigger costs no round
    /// trip.
    ///
    /// [`reconcile`]: SupervisorWSWorker::reconcile
    #[serde(with = "humantime_serde")]
    pub supervisor_reconcile_interval: Duration,
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
    host_id: Uuid,
    worker_instance_id: u64,
    config: SupervisorWSWorkerConfig,
    /// Log-streaming components, present iff the deployment enables the feature.
    /// Used to mint the per-job write token (pure, inside the dispatch txn) and
    /// to provision the per-job JetStream stream (NATS I/O, *outside* the row
    /// lock — see `reconcile`). `None` disables streaming: jobs dispatch with no
    /// `log_streaming` destination.
    log_streaming: Option<LogStreaming>,
}

pub struct SupervisorWSWorker<S: SupervisorSocket> {
    ctx: WorkerCtx,
    socket: S,
    /// The supervisor's last reported status (`J_sup`), refreshed out-of-band by
    /// the periodic `StatusRequest`/`StatusResponse` round trip (and, later, the
    /// event stream). `reconcile` converges against *this* cached value rather
    /// than forcing a fresh round trip, so a reconcile triggered by anything
    /// (the timer, a status response, a future Postgres pub/sub notification) is
    /// a pure DB operation. `None` until the first status is seen, during which
    /// reconcile is a no-op (the actual supervisor state is still unknown).
    last_seen_status: Option<ReportedSupervisorStatus>,
}

/// Error type for [`SupervisorWSWorker`] operations.
///
/// `Stale` is a distinct, non-fatal variant: it signals that a newer worker
/// has taken over for this host's supervisor and the current worker should
/// exit gracefully. Any other failure flows through `Other` and is treated as
/// a real error. `From<anyhow::Error>` lets call sites propagate ordinary
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

/// What `run_loop` should do after [`handle_supervisor_msg`] processes one
/// inbound message. Keeping the post-message action explicit (rather than
/// reconciling inside the handler) lets `run_loop` own the reconcile-timer reset
/// in one place, and keeps the handler free of the timer it doesn't own.
///
/// [`handle_supervisor_msg`]: SupervisorWSWorker::handle_supervisor_msg
enum PostMsg {
    /// Keep looping; nothing further to do.
    Continue,
    /// The cached supervisor status changed — run a reconcile pass.
    Reconcile,
    /// Terminate the worker loop (the peer closed or signalled a fatal error).
    Terminate,
}

impl WorkerCtx {
    async fn obtain_worker_instance_id(pool: &PgPool, host_id: Uuid) -> Result<u64> {
        let db_worker_instance_id: i64 = sql::host::increment_worker_instance_id(host_id, pool)
            .await
            .with_context(|| format!("Obtaining new worker instance ID for host {:?}", host_id))?;

        Ok(db_worker_instance_id.try_into().expect(
            "Database invariant violated: worker_instance_id must be zero or \
	     positive integer!",
        ))
    }

    /// Run `f` inside a transaction that holds an exclusive lock on this host's
    /// row for the duration of the closure.
    ///
    /// The lock serializes all worker writes for this host: a concurrent
    /// `increment_worker_instance_id` (issued by a takeover attempt from a new
    /// worker) will block until this transaction commits or rolls back. After
    /// taking the lock, this checks the host's `worker_instance_id`; if it no
    /// longer matches, the closure is not executed and [`WorkerError::Stale`]
    /// is returned so the caller can exit gracefully via `?`.
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
                "Beginning SupervisorWSWorker transaction for host {} \
                 (worker_instance_id {})",
                self.host_id, self.worker_instance_id,
            )
        })?;

        let current_worker_instance_id: u64 =
            sql::host::lock_and_get_current_worker(self.host_id, &mut txn)
                .await
                .with_context(|| {
                    format!(
                        "Locking and reading worker_instance_id for host {}",
                        self.host_id,
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
                "Committing SupervisorWSWorker transaction for host {} \
                 (worker_instance_id {})",
                self.host_id, self.worker_instance_id,
            )
        })?;

        Ok(out)
    }
}

impl<S: SupervisorSocket> SupervisorWSWorker<S> {
    #[tracing::instrument(skip(pool, socket, config, log_streaming))]
    pub async fn run(
        pool: PgPool,
        host_id: Uuid,
        socket: S,
        config: SupervisorWSWorkerConfig,
        log_streaming: Option<LogStreaming>,
    ) {
        match Self::run_inner(pool, host_id, socket, config, log_streaming).await {
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
        host_id: Uuid,
        socket: S,
        config: SupervisorWSWorkerConfig,
        log_streaming: Option<LogStreaming>,
    ) -> WorkerResult<()> {
        // A supervisor has just opened a new WebSocket connection for this host
        // and successfully authenticated. This might be because it's a new
        // supervisor, because it restarted, or because the connection was
        // interrupted.
        //
        // At any given time, a host must have at most one switchboard worker
        // (this very method) responsible for managing the supervisor
        // connection. However, in the case of network issues etc., it might be
        // that there is another task that believes to still be responsible for
        // this host.
        //
        // To solve these issues, we store a monotonically increasing "worker
        // instance ID" counter per host. Each new worker instance first
        // atomically increments and obtains a unique worker instance ID, and
        // then runs any database operation inside `with_txn`, which holds an
        // exclusive row lock on the host record for the duration of the
        // transaction and verifies the worker is still current as its first
        // statement. The row lock serializes worker-vs-worker contention: a
        // newer worker's `increment_worker_instance_id` blocks until any
        // in-flight `with_txn` from the previous worker commits or rolls back.
        // If, on locking the row, the current worker observes a mismatched
        // ID, the transaction is rolled back and `WorkerError::Stale` is
        // returned, signalling the worker to exit gracefully.

        // This should never fail, as the supervisor has successfully
        // authenticated:
        let worker_instance_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;

        // Now, using this ID, construct the worker instance:
        let mut worker = SupervisorWSWorker {
            ctx: WorkerCtx {
                pool,
                host_id,
                worker_instance_id,
                config,
                log_streaming,
            },
            socket,
            last_seen_status: None,
        };

        let result = worker.run_loop().await;

        // Clean disconnect: mark the host not-live right away so the scheduler
        // stops dispatching to it without waiting out the heartbeat staleness
        // window. This goes through `with_txn`, so if we exited *because* a newer
        // worker superseded us, the guard short-circuits and we leave its fresh
        // heartbeat untouched. Ignore the outcome (incl. `Stale`) and surface
        // only the run-loop result; a silent worker death that skips this path
        // is still caught by heartbeat staleness.
        if let Err(e) = worker
            .ctx
            .with_txn(async |txn| {
                sql::host::mark_dead(host_id, txn).await?;
                Ok(())
            })
            .await
        {
            tracing::debug!("SupervisorWSWorker clean-disconnect mark_dead skipped: {e}");
        }

        result
    }

    /// Converge the supervisor's actual state to the switchboard's desired state.
    ///
    /// # Principle
    ///
    /// The **switchboard** is the source of truth for what is *assigned*
    /// (`hosts.current_job`, plus that job's `job_state`); the **supervisor** is
    /// ground truth for what is *physically executing*. Reconciliation drives the
    /// latter toward the former — issuing `StartJob` for newly-scheduled jobs,
    /// adopting reported running states, finalizing drops/exits, and stopping
    /// zombies.
    ///
    /// The reported supervisor status (`J_sup`) is read from the in-memory
    /// [`last_seen_status`] cache, **not** by issuing a fresh `StatusRequest`:
    /// the cache is refreshed out-of-band by the periodic status round trip (and,
    /// later, the event stream), so a reconcile is a pure DB operation regardless
    /// of what triggered it (the timer, an inbound status response, a future
    /// Postgres pub/sub notification). While the cache is `None` (no status seen
    /// yet) reconcile is a no-op — the actual state is unknown.
    ///
    /// Let `J_sb = hosts.current_job`, `S = its job_state`, and `J_sup` = the
    /// reported `OngoingJob` id, if any. The convergence table, split by `S`
    /// because a never-started (`scheduled`), a was-running
    /// (`initializing`/`ready`/`terminating`), and an already-terminal
    /// (`finalized`) job each mean something different when the supervisor is
    /// `Idle`:
    ///
    /// | `J_sb` (state)     | Supervisor reports     | Resolution |
    /// |--------------------|------------------------|------------|
    /// | none               | `Idle`                 | aligned; no action |
    /// | none               | `OngoingJob(J)`        | zombie → `StopJob(J)` |
    /// | `(J, scheduled)`   | `Idle`                 | not started yet → `StartJob(J)` (idempotent re-send; no DB change) |
    /// | `(J, scheduled)`   | `OngoingJob(J)`        | picked up → adopt reported `job_state` |
    /// | `(J, scheduled)`   | `OngoingJob(J'≠J)`     | foreign zombie → `StopJob(J')`; leave `J` scheduled for the next pass to start |
    /// | `(J, running)`     | `Idle`                 | dropped → finalize `host_dropped_job` (+ `RestartPolicy`) |
    /// | `(J, running)`     | `OngoingJob(J)`        | adopt reported `job_state`; a `Terminated` finalizes + `StopJob(J)` ack |
    /// | `(J, running)`     | `OngoingJob(J'≠J)`     | finalize `J` dropped (+ `RestartPolicy`); `StopJob(J')` |
    /// | `(J, finalized)`   | `Idle`                 | terminal, host released it → clear assignment |
    /// | `(J, finalized)`   | `OngoingJob(J)`        | terminal, host still retains the record → `StopJob(J)` ack; **keep** the assignment until the host reports it gone |
    /// | `(J, finalized)`   | `OngoingJob(J'≠J)`     | terminal, host has moved on → clear assignment; `StopJob(J')` |
    ///
    /// # The `finalized` rows: sticky-host recovery
    ///
    /// A job can be `finalized` while `hosts.current_job` still points at it: a
    /// job that errors or is canceled finalizes terminally, but the host pointer
    /// is only released once the host reports the job gone (a `Terminated`
    /// transition, or — via these rows — a reconcile that observes it gone). If
    /// the host never follows up (e.g. it errors out without reporting
    /// `Terminated`), the assignment would otherwise stay stuck forever. These
    /// rows recover from that: the job row is already terminal, so reconcile
    /// **never** re-finalizes it, adopts a running state over it (which would
    /// un-finalize the row), or applies the restart policy — it only drives the
    /// supervisor to drop the job (`StopJob`) and releases the pointer once the
    /// reported status confirms the supervisor no longer holds it. The column
    /// thus stays a faithful mirror of the supervisor's actual assignment.
    ///
    /// # Idempotence
    ///
    /// Every resolution is either an idempotent command (`StartJob`/`StopJob`
    /// carry the `job_id` and are no-ops when they don't apply to the current job
    /// state) or a `job_state`-guarded DB transition, so a replayed reconcile
    /// converges to the same state. All DB writes go through
    /// [`WorkerCtx::with_txn`] so the takeover/staleness guard covers them; the
    /// adopt rows share [`sql::job::apply_running_state`] with the event path so
    /// the running-state mapping lives in one place, and the drop transitions are
    /// `sql::job::finalize_dropped_and_maybe_restart`.
    ///
    /// The `Terminated` fold: a reported `RunningJobState::Terminated` does not
    /// map to a live `job_state`; `apply_running_state` instead finalizes the job
    /// with `termination_reason = workload_exited` (**no** restart — a clean exit
    /// is not a failure) and signals that the worker must `StopJob`-ack, which the
    /// supervisor uses to drop its retained terminal record. The task outcome is
    /// preserved as last declared out-of-band via `apply_task_outcome`. See the
    /// `RunningJobState` Rustdoc in `treadmill_rs::api::switchboard_supervisor`
    /// for the retained-terminal contract.
    ///
    /// The `tests` submodule specifies each row above.
    ///
    /// [`last_seen_status`]: SupervisorWSWorker::last_seen_status
    async fn reconcile(&mut self) -> WorkerResult<()> {
        use crate::sql::job::SqlJobState;

        // Read the supervisor's actual state from the cache; until we have one,
        // there is nothing to converge against.
        let Some(reported) = self.last_seen_status.clone() else {
            return Ok(());
        };

        let host_id = self.ctx.host_id;
        let at = chrono::Utc::now();

        // The log-streaming config (token minting only) is read inside the txn;
        // stream provisioning (NATS I/O) happens *after* commit, below.
        let log_streaming_config = self.ctx.log_streaming.as_ref().map(|ls| ls.config.clone());

        // All reads and writes run under the takeover/staleness guard. The
        // closure returns the single command (if any) the worker must send
        // *after* the transaction commits: we never await the socket while the
        // host row lock is held (see `with_txn`'s contract).
        let command: Option<SwitchboardToSupervisor> = self
            .ctx
            .with_txn(async move |txn| {
                let j_sb = sql::host::fetch_current_job(host_id, txn).await?;

                let command = match j_sb {
                    // Nothing assigned: aligned if idle, else a zombie to stop.
                    None => match reported {
                        ReportedSupervisorStatus::Idle => None,
                        ReportedSupervisorStatus::OngoingJob { job_id, .. } => {
                            Some(SwitchboardToSupervisor::StopJob(StopJobMessage { job_id }))
                        }
                    },

                    // A job is assigned: its `job_state` decides what `Idle`
                    // (and a same-id report) means.
                    Some(id) => {
                        let job = sql::job::fetch_by_job_id(id, &mut **txn).await?;

                        // Finalized-but-still-assigned: the job reached a terminal
                        // state out-of-band (e.g. finalized via a
                        // `SupervisorJobEvent::Error`, or a `Terminated` whose ack
                        // is still in flight) but the host pointer was never
                        // released. The job row is already terminal, so we never
                        // re-finalize, adopt a running state (which would
                        // un-finalize it), or apply the restart policy: we only
                        // drive the supervisor to drop the job and release the
                        // pointer once the report confirms it is gone, keeping the
                        // column a faithful mirror of the supervisor (see the
                        // `finalized` rows in the convergence table above).
                        if job.job_state() == SqlJobState::Finalized {
                            return Ok(match reported {
                                // The supervisor confirms it no longer holds the
                                // job: now (and only now) release the pointer.
                                ReportedSupervisorStatus::Idle => {
                                    sql::host::release_job_assignment(host_id, id, txn).await?;
                                    None
                                }

                                // The supervisor still reports *our* job — in any
                                // state, including a retained `Terminated` record.
                                // `StopJob`-ack to make it drop the record, but
                                // keep the assignment until it reports gone.
                                ReportedSupervisorStatus::OngoingJob { job_id: j_sup, .. }
                                    if j_sup == id =>
                                {
                                    Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                        job_id: id,
                                    }))
                                }

                                // The supervisor reports a *different* job: ours is
                                // demonstrably gone from the host, so release our
                                // pointer; the reported one is an unassigned zombie.
                                ReportedSupervisorStatus::OngoingJob { job_id: j_sup, .. } => {
                                    sql::host::release_job_assignment(host_id, id, txn).await?;
                                    Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                        job_id: j_sup,
                                    }))
                                }
                            });
                        }

                        // Stop pre-check: should this assigned job be stopped for
                        // a switchboard-side reason (execution timeout or user
                        // cancel)? Re-derived from the freshly-fetched row on
                        // every pass, so an extended deadline (or a cancel signal
                        // set since the last pass) is always honored against
                        // current DB state — we never terminate on stale data.
                        if let Some(reason) = job.switchboard_stop_reason(at) {
                            match reported {
                                // The supervisor no longer runs it (already gone,
                                // or it never started): finalize with the reason
                                // and release the assignment (the supervisor
                                // confirms it is gone).
                                ReportedSupervisorStatus::Idle => {
                                    sql::job::finalize_with_reason(id, reason, at, txn).await?;
                                    sql::host::release_job_assignment(host_id, id, txn).await?;
                                    None
                                }

                                // The supervisor still reports *this* job.
                                ReportedSupervisorStatus::OngoingJob {
                                    job_id: j_sup,
                                    job_state,
                                } if j_sup == id => match job_state {
                                    // Already torn down, but the supervisor still
                                    // retains the terminal record: finalize with the
                                    // reason and ack with StopJob, but keep the
                                    // assignment until it reports the job gone (the
                                    // `finalized` rows release it then).
                                    RunningJobState::Terminated => {
                                        sql::job::finalize_with_reason(id, reason, at, txn).await?;
                                        Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                            job_id: id,
                                        }))
                                    }
                                    // Still running: track the reported state and
                                    // (re)issue StopJob until it reports gone.
                                    running => {
                                        sql::job::apply_running_state(id, running, at, txn).await?;
                                        Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                            job_id: id,
                                        }))
                                    }
                                },

                                // The supervisor runs a *different* job: ours is
                                // gone (finalize with the reason, release the
                                // assignment); the reported one is an unassigned
                                // zombie to stop.
                                ReportedSupervisorStatus::OngoingJob { job_id: j_sup, .. } => {
                                    sql::job::finalize_with_reason(id, reason, at, txn).await?;
                                    sql::host::release_job_assignment(host_id, id, txn).await?;
                                    Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                        job_id: j_sup,
                                    }))
                                }
                            }
                        } else {
                            match (job.job_state(), reported) {
                                // Scheduled + Idle: the supervisor hasn't picked it
                                // up yet — (re)dispatch it. No DB change; the next
                                // pass adopts the reported running state.
                                (SqlJobState::Scheduled, ReportedSupervisorStatus::Idle) => {
                                    let msg = sql::job::build_start_job_message(
                                        &job,
                                        txn,
                                        log_streaming_config.as_ref(),
                                    )
                                    .await
                                    .context("building StartJob message in reconcile")?;
                                    Some(SwitchboardToSupervisor::StartJob(msg))
                                }

                                // Scheduled + the supervisor reports *this* job: it
                                // picked it up — adopt the reported running state
                                // (scheduled → initializing/ready/...).
                                (
                                    SqlJobState::Scheduled,
                                    ReportedSupervisorStatus::OngoingJob {
                                        job_id: j_sup,
                                        job_state,
                                    },
                                ) if j_sup == id => {
                                    let terminated =
                                        sql::job::apply_running_state(id, job_state, at, txn)
                                            .await?;
                                    terminated.then_some(SwitchboardToSupervisor::StopJob(
                                        StopJobMessage { job_id: id },
                                    ))
                                }

                                // Scheduled, but the supervisor is busy with some
                                // *other* job: that one is a zombie. Stop it and
                                // leave `id` scheduled for the next pass to start.
                                (
                                    SqlJobState::Scheduled,
                                    ReportedSupervisorStatus::OngoingJob { job_id: j_sup, .. },
                                ) => Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                    job_id: j_sup,
                                })),

                                // Running (initializing/ready/terminating) + Idle:
                                // the job was lost — finalize (honoring the restart
                                // policy) and release the assignment (the supervisor
                                // confirms it is gone).
                                (_, ReportedSupervisorStatus::Idle) => {
                                    sql::job::finalize_dropped_and_maybe_restart(id, at, txn)
                                        .await?;
                                    sql::host::release_job_assignment(host_id, id, txn).await?;
                                    None
                                }

                                // Running + the supervisor reports *this* job: adopt
                                // the reported state; a `Terminated` finalizes and is
                                // acked with `StopJob` (the assignment is kept, then
                                // released by the `finalized` rows once the host
                                // reports the job gone).
                                (
                                    _,
                                    ReportedSupervisorStatus::OngoingJob {
                                        job_id: j_sup,
                                        job_state,
                                    },
                                ) if j_sup == id => {
                                    let terminated =
                                        sql::job::apply_running_state(id, job_state, at, txn)
                                            .await?;
                                    terminated.then_some(SwitchboardToSupervisor::StopJob(
                                        StopJobMessage { job_id: id },
                                    ))
                                }

                                // Running, but the supervisor reports a *different*
                                // job: the assigned one is lost (finalize + restart
                                // policy, release the assignment); the reported one
                                // is an unassigned zombie.
                                (_, ReportedSupervisorStatus::OngoingJob { job_id: j_sup, .. }) => {
                                    sql::job::finalize_dropped_and_maybe_restart(id, at, txn)
                                        .await?;
                                    sql::host::release_job_assignment(host_id, id, txn).await?;
                                    Some(SwitchboardToSupervisor::StopJob(StopJobMessage {
                                        job_id: j_sup,
                                    }))
                                }
                            }
                        }
                    }
                };

                Ok(command)
            })
            .await?;

        if let Some(command) = command {
            // Provision the per-job JetStream stream before dispatching a
            // StartJob that carries a log-streaming destination, so the stream
            // exists by the time the supervisor connects and publishes. Done
            // here, *after* the txn commits, because it is NATS I/O and must not
            // run while the host row lock is held (see `with_txn`'s contract).
            // Idempotent: reconcile re-sends StartJob, and an existing stream is
            // a success.
            let stream_to_provision = match &command {
                SwitchboardToSupervisor::StartJob(msg) if msg.log_streaming.is_some() => {
                    Some(msg.job_id)
                }
                _ => None,
            };
            if let (Some(job_id), Some(log_streaming)) =
                (stream_to_provision, self.ctx.log_streaming.as_ref())
            {
                log_streaming
                    .provisioner
                    .ensure_job_stream(job_id)
                    .await
                    .context("provisioning JetStream stream before dispatch")?;
            }
            self.send_command(command).await?;
        }

        Ok(())
    }

    /// Record a task outcome the supervisor declared via
    /// [`SupervisorJobEvent::DeclareExitStatus`], out-of-band of reconciliation.
    ///
    /// The write only lands while the job is dispatched to this host
    /// (`sql::job::set_task_outcome` is guarded on the assignment pointer);
    /// returns `false` if the job is not currently assigned here (e.g. already
    /// finalized or never dispatched), in which case the event is dropped.
    ///
    /// [`SupervisorJobEvent::DeclareExitStatus`]: treadmill_rs::api::switchboard_supervisor::SupervisorJobEvent::DeclareExitStatus
    async fn apply_task_outcome(
        &mut self,
        job_id: Uuid,
        outcome: TaskExitStatus,
        message: Option<String>,
    ) -> WorkerResult<bool> {
        let host_id = self.ctx.host_id;
        let applied = self
            .ctx
            .with_txn(async move |txn| {
                Ok(
                    sql::job::set_task_outcome(job_id, host_id, outcome.into(), message, txn)
                        .await?,
                )
            })
            .await?;
        Ok(applied)
    }

    /// Serialize a [`SwitchboardToSupervisor`] command as JSON and send it over
    /// the socket as a Text frame (the protocol is JSON-over-Text; see the
    /// protocol module).
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
    ) -> WorkerResult<PostMsg> {
        match msg {
            ws::Message::Close(_close_frame) => {
                tracing::info!("SupervisorWSWorker: supervisor sent close message, terminating.");
                Ok(PostMsg::Terminate)
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
                Ok(PostMsg::Continue)
            }

            ws::Message::Binary(payload) => {
                tracing::warn!(
                    "SupervisorWSWorker: received unexpected binary WebSocket message ({} bytes), discarding...",
                    payload.len()
                );
                Ok(PostMsg::Continue)
            }

            ws::Message::Text(payload) => {
                tracing::trace!(
                    "SupervisorWSWorker: received text WebSocket message ({} bytes)",
                    payload.len()
                );

                match serde_json::from_str::<SupervisorToSwitchboard>(&payload) {
                    // A fresh status snapshot: refresh the cache and converge
                    // against it. This is the out-of-band refresh `reconcile`
                    // relies on (it never requests status itself).
                    Ok(SupervisorToSwitchboard::StatusResponse(response)) => {
                        tracing::trace!(?response, "received StatusResponse from supervisor");
                        self.last_seen_status = Some(response.message);
                        Ok(PostMsg::Reconcile)
                    }

                    // Asynchronous job events (state transitions, task outcomes,
                    // errors) are mirrored into the DB out-of-band of
                    // reconciliation, and keep the status cache fresh.
                    Ok(SupervisorToSwitchboard::SupervisorEvent(event)) => {
                        tracing::trace!(?event, "received SupervisorEvent from supervisor");
                        self.handle_supervisor_event(event).await
                    }

                    // The supervisor signalled a fatal protocol error: log and
                    // terminate (the connector reconnects). See `ProtocolError`.
                    Ok(SupervisorToSwitchboard::ProtocolError(err)) => {
                        tracing::warn!(
                            ?err,
                            "SupervisorWSWorker: supervisor reported a protocol error, terminating."
                        );
                        Ok(PostMsg::Terminate)
                    }

                    // A malformed message is non-fatal on our side: log and keep
                    // the connection alive (a genuinely dead peer is caught by
                    // the PONG keepalive).
                    Err(e) => {
                        tracing::warn!(
                            "SupervisorWSWorker: failed to decode text message from supervisor: {e}; discarding."
                        );
                        Ok(PostMsg::Continue)
                    }
                }
            }

            ws::Message::Pong(_) => {
                tracing::trace!("SupervisorWSWorker: received PONG from supervisor");
                pong_timeout
                    .as_mut()
                    .reset(Instant::now() + self.ctx.config.supervisor_pong_dead);
                Ok(PostMsg::Continue)
            }
        }
    }

    /// Send a `StatusRequest` to the supervisor. The correlated `StatusResponse`
    /// refreshes [`last_seen_status`] (see [`handle_supervisor_msg`]); we accept
    /// any response rather than tracking the id, since at most one request is
    /// outstanding at a time and a status snapshot is idempotent.
    ///
    /// [`last_seen_status`]: SupervisorWSWorker::last_seen_status
    /// [`handle_supervisor_msg`]: SupervisorWSWorker::handle_supervisor_msg
    async fn send_status_request(&mut self) -> WorkerResult<()> {
        self.send_command(SwitchboardToSupervisor::StatusRequest(Request {
            request_id: Uuid::new_v4(),
            message: (),
        }))
        .await
    }

    /// Mirror an asynchronous [`SupervisorEvent`] into the DB, out-of-band of
    /// reconciliation. These events make the lifecycle update in real time
    /// (rather than only at the next reconcile) and carry data not present in the
    /// status snapshot — most importantly the task outcome
    /// ([`SupervisorJobEvent::DeclareExitStatus`]). Each event is applied only
    /// while the job is the one assigned to this host; events for any other job
    /// are dropped.
    async fn handle_supervisor_event(&mut self, event: SupervisorEvent) -> WorkerResult<PostMsg> {
        let SupervisorEvent::JobEvent { job_id, event } = event;
        match event {
            // A reported running-state advance: adopt it through the same helper
            // reconcile uses, and keep the status cache in step.
            SupervisorJobEvent::StateTransition {
                new_state,
                status_message,
            } => {
                tracing::trace!(
                    ?new_state,
                    ?status_message,
                    "received StateTransition event from supervisor"
                );
                self.apply_state_transition(job_id, new_state).await
            }

            // The dedicated task-outcome channel; independent of lifecycle state.
            SupervisorJobEvent::DeclareExitStatus { outcome, message } => {
                tracing::trace!(
                    ?outcome,
                    ?message,
                    "received DeclareExitStatus event from supervisor"
                );
                if !self.apply_task_outcome(job_id, outcome, message).await? {
                    tracing::debug!(
                        %job_id,
                        "DeclareExitStatus for a job not assigned to this host; dropped"
                    );
                }
                Ok(PostMsg::Continue)
            }

            // A job-level error: finalize terminally with a mapped reason.
            SupervisorJobEvent::Error { error } => {
                tracing::warn!(?error, "received Error event from supervisor");
                self.finalize_job_error(job_id, error).await?;
                Ok(PostMsg::Continue)
            }
        }
    }

    /// Adopt a [`SupervisorJobEvent::StateTransition`] into the DB via the shared
    /// [`sql::job::apply_running_state`], but only while the job is the one
    /// assigned to this host (a stale/foreign event is dropped). Refreshes the
    /// status cache to the reported state, and `StopJob`-acks a `Terminated`.
    ///
    /// On `Terminated` the job finalizes but the assignment is **not** released
    /// here (see [`sql::job::finalize_terminated`]): the cache is set to the
    /// reported terminal state rather than synthesizing `Idle`, so the pointer is
    /// released only once the supervisor genuinely reports the job gone, via
    /// reconcile's `finalized` rows. This keeps `hosts.current_job` a faithful
    /// mirror of the supervisor and reuses the one sticky-host recovery path.
    async fn apply_state_transition(
        &mut self,
        job_id: Uuid,
        new_state: RunningJobState,
    ) -> WorkerResult<PostMsg> {
        let host_id = self.ctx.host_id;
        let at = chrono::Utc::now();
        let state_for_txn = new_state.clone();

        let outcome: Option<bool> = self
            .ctx
            .with_txn(async move |txn| {
                // Guard: only mirror events for the currently-assigned job.
                if sql::host::fetch_current_job(host_id, txn).await? != Some(job_id) {
                    return Ok(None);
                }
                Ok(Some(
                    sql::job::apply_running_state(job_id, state_for_txn, at, txn).await?,
                ))
            })
            .await?;

        let Some(terminated) = outcome else {
            tracing::debug!(
                %job_id,
                "StateTransition for a job not assigned to this host; dropped"
            );
            return Ok(PostMsg::Continue);
        };

        // Keep the cache in step with the supervisor's actual report so a later
        // reconcile sees the same state. A `Terminated` is cached as the reported
        // (retained) terminal record, not `Idle`: the assignment is released only
        // once the supervisor reports the job genuinely gone.
        self.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: new_state,
        });

        if terminated {
            self.send_command(SwitchboardToSupervisor::StopJob(StopJobMessage { job_id }))
                .await?;
        }
        Ok(PostMsg::Continue)
    }

    /// Finalize the job a [`SupervisorJobEvent::Error`] names, while it is the one
    /// assigned to this host (a stale/foreign event is dropped). Records the
    /// mapped [`sql::job::termination_reason_for_job_error`] and the error
    /// description.
    ///
    /// The assignment is **not** released and the status cache is **not** forced
    /// to `Idle`: an `Error` is reported out-of-band, before the supervisor's
    /// terminal `Terminated` transition, so the supervisor may still hold the job.
    /// Leaving the cache reflecting the supervisor's last actual report keeps
    /// `hosts.current_job` a faithful mirror — reconcile's `finalized` rows then
    /// `StopJob`-drive the supervisor and release the pointer once it reports the
    /// job gone (the sticky-host recovery path), instead of releasing eagerly here
    /// on an assumption the supervisor never confirmed.
    async fn finalize_job_error(
        &mut self,
        job_id: Uuid,
        error: treadmill_rs::connector::JobError,
    ) -> WorkerResult<()> {
        let host_id = self.ctx.host_id;
        let at = chrono::Utc::now();
        let reason = sql::job::termination_reason_for_job_error(&error.error_kind);
        let message = Some(error.description.clone());

        let finalized = self
            .ctx
            .with_txn(async move |txn| {
                // Guard: only finalize the currently-assigned job.
                if sql::host::fetch_current_job(host_id, txn).await? != Some(job_id) {
                    return Ok(false);
                }
                Ok(sql::job::finalize_errored(job_id, reason, message, at, txn).await?)
            })
            .await?;

        if finalized {
            tracing::info!(
                ?reason,
                message = error.description,
                "finalized job with error state"
            );
        } else {
            tracing::debug!(
                %job_id,
                "Error event for a job not assigned to this host; dropped"
            );
        }
        Ok(())
    }

    async fn run_loop(&mut self) -> WorkerResult<()> {
        // Keepalive PING message interval, for PING messages sent from the
        // switchboard to the supervisor:
        let mut ping_interval = interval(self.ctx.config.supervisor_ping_interval);

        // Dedicated reconcile cadence (see `supervisor_reconcile_interval`). It
        // is reset whenever a reconcile runs for another reason (the liveness
        // tick below, or an inbound status response), so it only fires to cover
        // stretches where nothing else triggered convergence.
        let mut reconcile_interval = interval(self.ctx.config.supervisor_reconcile_interval);

        // Timeout for waiting on PONG messages from the supervisor. Reset every
        // time we're getting a PONG response from the supervisor:
        let mut pong_timeout = Box::pin(sleep(self.ctx.config.supervisor_pong_dead));

        // Mark the host live immediately on connect, so the scheduler can
        // dispatch to it without waiting for the first ping tick. (Also doubles
        // as the takeover check: if a newer worker already superseded us between
        // claiming our instance ID and here, `with_txn` returns `Stale`.)
        let host_id = self.ctx.host_id;
        self.ctx
            .with_txn(async move |txn| {
                sql::host::touch_heartbeat(host_id, txn).await?;
                Ok(())
            })
            .await?;

        // Prime the status cache: the correlated response drives the initial
        // reconcile (and resets the reconcile timer) once it arrives.
        self.send_status_request().await?;

        loop {
            tokio::select! {
                // Receive incoming messages from the supervisor via WebSocket:
                opt_msg = self.socket.next() => {
                    let Some(res_msg) = opt_msg else {
                        tracing::info!("SupervisorWSWorker socket closed, terminating.");
                        return Ok(());
                    };

                    let msg = res_msg.context("SupervisorWSWorker reading message from supervisor WebSocket")?;

                    match self.handle_supervisor_msg(msg, &mut pong_timeout).await? {
                        PostMsg::Terminate => return Ok(()),
                        // A status response refreshed the cache: converge now,
                        // and reset the dedicated timer (this counts as the
                        // period's reconcile).
                        PostMsg::Reconcile => {
                            self.reconcile().await?;
                            reconcile_interval.reset();
                        }
                        PostMsg::Continue => {}
                    }
                }

                // Periodically sending a WebSocket PING message to the
                // supervisor. This does double duty:
                // - it keeps the connection from being marked as idle and can
                //   help us recognize dead supervisor connections;
                // - we use it to check that there is not a newer worker serving
                //   this supervisor, in which case we terminate.
                _ = ping_interval.tick() => {
                    tracing::trace!("sending ping heartbeat");

                    // Refresh the liveness heartbeat. This transaction also
                    // determines whether we're still the most current worker
                    // (its first statement is the staleness check), so it does
                    // double duty as the takeover guard.
                    self.ctx.with_txn(async move |txn| {
                        sql::host::touch_heartbeat(host_id, txn).await?;
                        Ok(())
                    }).await?;

                    // Still current, now send a PING:
                    self.socket.send(ws::Message::Ping((&[][..]).into())).await.context("SupervisorWSWorker sending ping to supervisor")?;

                    // Piggyback the regular status refresh on the liveness tick
                    // (its response drives the next reconcile). This is also the
                    // "reset on liveness check" point: the reconcile timer need
                    // not fire on top of the refresh we just kicked off.
                    self.send_status_request().await?;
                    reconcile_interval.reset();
                }

                // Dedicated reconcile cadence: converge against the cached
                // status without forcing a status request (covers stretches
                // where neither the liveness tick nor a status response fired).
                _ = reconcile_interval.tick() => {
                    tracing::trace!("performing reconcile");
                    self.reconcile().await?;
                }

                // Timeout for waiting on a PONG message from the supervisor. If
                // we haven't received one within the timeout and this fires, it
                // means we should consider the connection dead.
                _ = &mut pong_timeout => {
                    tracing::trace!("reached pong timeout");
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

    use super::*;
    use crate::auth::token::SecurityToken;
    use std::collections::BTreeSet;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};
    use treadmill_rs::api::switchboard_supervisor::{
        ImageSpecification, RunningJobState, SupervisorEvent, SupervisorJobEvent,
        SwitchboardToSupervisor, TaskExitStatus,
    };
    use treadmill_rs::connector::{JobError, JobErrorKind};

    /// Build a `SupervisorWSWorkerConfig` with the given ping interval /
    /// dead-pong deadline, both expressed in milliseconds. Tests use short
    /// values so the suite stays fast without hard-coding magic durations
    /// everywhere.
    fn worker_config(ping_interval_ms: u64, pong_dead_ms: u64) -> SupervisorWSWorkerConfig {
        SupervisorWSWorkerConfig {
            supervisor_ping_interval: Duration::from_millis(ping_interval_ms),
            supervisor_pong_dead: Duration::from_millis(pong_dead_ms),
            // Effectively disabled by default: tests that exercise the dedicated
            // reconcile cadence set their own short interval. Keeping it huge
            // here means the ping/pong/inbound tests don't see stray reconcile
            // ticks (their reconciles, if any, are driven directly).
            supervisor_reconcile_interval: Duration::from_secs(3600),
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

    async fn insert_host(pool: &PgPool) -> anyhow::Result<Uuid> {
        let host_id = Uuid::new_v4();
        sql::host::insert(
            host_id,
            format!("test-host-{host_id}"),
            SecurityToken::generate(),
            &BTreeSet::new(),
            Vec::new(),
            pool,
        )
        .await?;
        Ok(host_id)
    }

    fn worker(
        pool: PgPool,
        host_id: Uuid,
        worker_instance_id: u64,
        config: SupervisorWSWorkerConfig,
    ) -> SupervisorWSWorker<NoSocket> {
        SupervisorWSWorker {
            ctx: WorkerCtx {
                pool,
                host_id,
                worker_instance_id,
                config,
                log_streaming: None,
            },
            socket: NoSocket,
            last_seen_status: None,
        }
    }

    /// Like [`worker`], but over a [`ScriptedSocket`] so a test can observe the
    /// worker's outgoing commands on the returned receiver. Used by the
    /// reconciliation tests, which assert that `StopJob`/`StartJob` are emitted.
    fn scripted_worker(
        pool: PgPool,
        host_id: Uuid,
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
                host_id,
                worker_instance_id,
                config,
                log_streaming: None,
            },
            socket,
            last_seen_status: None,
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
             (token_id, token, user_id, revoked, created_at, expires_at) \
             values ($1, $2, $3, null, now(), now() + interval '1 day')",
        )
        .bind(token_id)
        .bind(vec![0u8; 32])
        .bind(user_id)
        .execute(pool)
        .await?;
        Ok(token_id)
    }

    /// Register a throwaway concrete image (unique digest, no registry location)
    /// and return its id. Concrete-image jobs reference a real `images` row to
    /// satisfy the `image_id` FK and `valid_init_spec`; a per-call id keeps the
    /// `manifest_digest` unique across repeated inserts in one test.
    async fn insert_image(pool: &PgPool) -> anyhow::Result<Uuid> {
        let image_id = Uuid::new_v4();
        let id_hex = image_id.simple().to_string();
        let digest = format!("sha256:{id_hex:0>64}");
        sqlx::query(
            "insert into tml_switchboard.images \
             (id, manifest_digest, artifact_type, owner_subject, attrs) \
             values ($1, $2, 'application/vnd.oci.image.manifest.v1+json', null, '{}'::jsonb)",
        )
        .bind(image_id)
        .bind(digest)
        .execute(pool)
        .await?;
        Ok(image_id)
    }

    /// Insert a job already bound to `host_id` in the given `job_state`
    /// (e.g. `"scheduled"`, `"ready"`). `started_at` is set iff the state is one
    /// of the executing states, matching the `started_at_iso_executing` CHECK.
    /// `remaining_restarts` seeds the restart policy (cases 3/5 honor it).
    async fn insert_job(
        pool: &PgPool,
        token_id: Uuid,
        host_id: Uuid,
        job_state: &str,
        remaining_restarts: i32,
    ) -> anyhow::Result<Uuid> {
        let job_id = Uuid::new_v4();
        let image_id = insert_image(pool).await?;
        sqlx::query(
            "insert into tml_switchboard.jobs \
             (job_id, resume_job_id, restart_job_id, image_id, image_group_id, \
              image_group_generation, ssh_keys, \
              restart_policy, enqueued_by_token_id, host_tag_requirements, job_timeout, job_state, \
              initializing_stage, queued_at, started_at, dispatched_on_host_id, ssh_endpoints, \
              termination_reason, task_exit_status, exit_message, terminated_at, last_updated_at) \
             values \
             ($1, null, null, $2, null, null, '{}'::text[], row($3)::tml_switchboard.restart_policy, \
              $4, '{}'::text[], interval '1 hour', $5::tml_switchboard.job_state, null, \
              now(), \
              case when $5::tml_switchboard.job_state \
                        in ('initializing', 'ready', 'terminating') \
                   then now() else null end, \
              $6, null, \
              null, null, null, null, default)",
        )
        .bind(job_id)
        .bind(image_id)
        .bind(remaining_restarts)
        .bind(token_id)
        .bind(job_state)
        .bind(host_id)
        .execute(pool)
        .await?;
        Ok(job_id)
    }

    /// Point `host_id.current_job` at `job_id` (or clear it with `None`).
    async fn set_current_job(
        pool: &PgPool,
        host_id: Uuid,
        job_id: Option<Uuid>,
    ) -> anyhow::Result<()> {
        sqlx::query("update tml_switchboard.hosts set current_job = $2 where host_id = $1")
            .bind(host_id)
            .bind(job_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Insert a job already in the terminal `finalized` state and point
    /// `host_id.current_job` at it, reproducing the "finalized but still
    /// assigned" stuck state: the job is terminal (so `dispatched_on_host_id` is
    /// null per the `dispatched_host_iso_assigned` invariant, and
    /// `termination_reason`/`terminated_at` are set per the finalized CHECKs) yet
    /// the host pointer was never released. This is the state a job lands in when
    /// it finalizes (e.g. via a `SupervisorJobEvent::Error`) but the host never
    /// reports the `Terminated` transition that normally clears the assignment.
    async fn insert_finalized_assigned_job(
        pool: &PgPool,
        token_id: Uuid,
        host_id: Uuid,
    ) -> anyhow::Result<Uuid> {
        let job_id = Uuid::new_v4();
        let image_id = insert_image(pool).await?;
        sqlx::query(
            "insert into tml_switchboard.jobs \
             (job_id, resume_job_id, restart_job_id, image_id, image_group_id, \
              image_group_generation, ssh_keys, \
              restart_policy, enqueued_by_token_id, host_tag_requirements, job_timeout, job_state, \
              initializing_stage, queued_at, started_at, dispatched_on_host_id, ssh_endpoints, \
              termination_reason, task_exit_status, exit_message, terminated_at, last_updated_at) \
             values \
             ($1, null, null, $2, null, null, '{}'::text[], row(0)::tml_switchboard.restart_policy, \
              $3, '{}'::text[], interval '1 hour', 'finalized', null, \
              now(), null, null, null, \
              'workload_exited', null, null, now(), default)",
        )
        .bind(job_id)
        .bind(image_id)
        .bind(token_id)
        .execute(pool)
        .await?;
        set_current_job(pool, host_id, Some(job_id)).await?;
        Ok(job_id)
    }

    /// Read back `hosts.current_job`.
    async fn current_job_of(pool: &PgPool, host_id: Uuid) -> anyhow::Result<Option<Uuid>> {
        Ok(
            sqlx::query_scalar("select current_job from tml_switchboard.hosts where host_id = $1")
                .bind(host_id)
                .fetch_one(pool)
                .await?,
        )
    }

    /// Whether the host is currently marked live (`last_seen_at IS NOT NULL`).
    async fn host_is_live(pool: &PgPool, host_id: Uuid) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar(
            "select last_seen_at is not null from tml_switchboard.hosts where host_id = $1",
        )
        .bind(host_id)
        .fetch_one(pool)
        .await?)
    }

    /// Force `last_seen_at` to a fixed non-null instant, standing in for a
    /// (notional) newer worker's heartbeat in the takeover-guard test.
    async fn set_heartbeat_now(pool: &PgPool, host_id: Uuid) -> anyhow::Result<()> {
        sqlx::query("update tml_switchboard.hosts set last_seen_at = now() where host_id = $1")
            .bind(host_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Poll `cond` until it returns true or `budget` elapses (for asserting on
    /// state a spawned worker updates asynchronously).
    async fn wait_until<F, Fut>(budget: std::time::Duration, mut cond: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if cond().await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
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
        let host_id = insert_host(&pool).await?;
        for expected in 1u64..=3 {
            let got = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
            assert_eq!(got, expected);
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn with_txn_runs_closure_and_commits_when_current(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let worker_instance_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let worker = worker(
            pool.clone(),
            host_id,
            worker_instance_id,
            worker_config(50, 250),
        );

        let renamed = "renamed-inside-txn".to_string();
        let returned = worker
            .ctx
            .with_txn(async |txn| {
                sqlx::query!(
                    r#"
                    UPDATE tml_switchboard.hosts
                    SET name = $2
                    WHERE host_id = $1
                    "#,
                    host_id,
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
            "SELECT name FROM tml_switchboard.hosts WHERE host_id = $1",
            host_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(observed, renamed, "transaction should have committed");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn with_txn_returns_stale_when_superseded(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let our_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        // Simulate a takeover: a newer worker bumps the instance ID.
        let newer_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        assert_eq!(newer_id, our_id + 1);

        let worker = worker(pool.clone(), host_id, our_id, worker_config(50, 250));
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
        let host_id = insert_host(&pool).await?;
        let worker_instance_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let worker = worker(
            pool.clone(),
            host_id,
            worker_instance_id,
            worker_config(50, 250),
        );

        let result: WorkerResult<()> = worker
            .ctx
            .with_txn(async |txn| {
                sqlx::query!(
                    "UPDATE tml_switchboard.hosts SET name = $2 WHERE host_id = $1",
                    host_id,
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
            "SELECT name FROM tml_switchboard.hosts WHERE host_id = $1",
            host_id,
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
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Peer sends Close as its first (and only) frame.
        to_worker
            .send(Ok(ws::Message::Close(None)))
            .expect("scripted inbound channel must be open");

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            SupervisorWSWorker::run(pool, host_id, socket, worker_config(50, 250), None),
        )
        .await
        .expect("worker should return promptly after peer Close");

        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_returns_when_socket_stream_ends(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Peer disappears without a Close frame — stream end.
        drop(to_worker);

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            SupervisorWSWorker::run(pool, host_id, socket, worker_config(50, 250), None),
        )
        .await
        .expect("worker should return promptly on socket stream end");

        Ok(())
    }

    // -- liveness heartbeat -------------------------------------------------
    //
    // The host's `last_seen_at` is the DB-only signal by which the
    // (out-of-process) scheduler learns a host has a live supervisor. A
    // connected worker sets it immediately and refreshes it each tick; a clean
    // disconnect clears it at once; a worker that has been superseded must not
    // touch it (the newer worker owns the heartbeat).

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn connect_marks_live_and_clean_disconnect_marks_dead(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        assert!(
            !host_is_live(&pool, host_id).await?,
            "a host with no worker starts not-live"
        );

        let (to_worker, _from_worker, socket) = scripted_socket();
        let jh = tokio::spawn(SupervisorWSWorker::run(
            pool.clone(),
            host_id,
            socket,
            worker_config(50, 5_000),
            None,
        ));

        // The worker marks the host live on connect (before the first ping).
        assert!(
            wait_until(std::time::Duration::from_secs(2), || async {
                host_is_live(&pool, host_id).await.unwrap_or(false)
            })
            .await,
            "worker should mark the host live shortly after connecting"
        );

        // Clean disconnect (stream end): the worker clears the heartbeat as it
        // exits, so the host is immediately not-live.
        drop(to_worker);
        tokio::time::timeout(std::time::Duration::from_secs(2), jh)
            .await
            .expect("worker should return promptly on stream end")
            .expect("worker task should not panic");
        assert!(
            !host_is_live(&pool, host_id).await?,
            "a clean disconnect should mark the host dead immediately"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn superseded_worker_cannot_mark_host_dead(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let our_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        // A newer worker takes over (bumps the instance ID) and is keeping the
        // heartbeat fresh.
        let _newer_id = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        set_heartbeat_now(&pool, host_id).await?;

        // Our now-stale worker hits its clean-disconnect path. The `with_txn`
        // guard must reject it as `Stale` and leave the heartbeat untouched.
        let worker = worker(pool.clone(), host_id, our_id, worker_config(50, 250));
        let res = worker
            .ctx
            .with_txn(async |txn| {
                sql::host::mark_dead(host_id, txn).await?;
                Ok(())
            })
            .await;
        assert!(
            matches!(res, Err(WorkerError::Stale { .. })),
            "a superseded worker's mark_dead must be rejected as Stale"
        );
        assert!(
            host_is_live(&pool, host_id).await?,
            "the superseded worker must not clobber the newer worker's heartbeat"
        );
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
        let host_id = insert_host(&pool).await?;
        let (to_worker, mut from_worker, socket) = scripted_socket();

        // 50ms ping cadence, 5s dead-pong deadline — the deadline is wide
        // enough that the dead-peer guard never fires during this test.
        let cfg = worker_config(50, 5_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

        for n in 1..=3 {
            // The worker also emits `StatusRequest` (Text) frames on connect and
            // on each ping tick; skip them — we only assert on the PINGs here.
            loop {
                let msg =
                    tokio::time::timeout(std::time::Duration::from_secs(2), from_worker.recv())
                        .await
                        .unwrap_or_else(|_| panic!("expected PING #{n} within 2s"))
                        .expect("from_worker channel must remain open while worker runs");
                match msg {
                    ws::Message::Ping(_) => break,
                    ws::Message::Text(_) => continue,
                    other => panic!("expected Ping or StatusRequest #{n}, got {other:?}"),
                }
            }
        }

        // Tidy up: closing inbound ends the worker.
        drop(to_worker);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), jh).await;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn run_exits_when_no_pong_within_dead_threshold(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Tight deadline — worker should declare the peer dead within ~200ms.
        let cfg = worker_config(50, 200);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

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
        let host_id = insert_host(&pool).await?;
        let (to_worker, mut from_worker, socket) = scripted_socket();

        // 50ms pings, 400ms dead-pong deadline: a missed Pong would kill the
        // worker within ~400ms. We reply promptly across ~800ms (two dead-pong
        // windows), then close cleanly; the worker must outlive both. The
        // deadline is generous on purpose — under parallel test load a single
        // heartbeat transaction can briefly stall the loop, and we don't want a
        // false dead-peer detection to flake this liveness check (the exact
        // threshold is pinned by `run_exits_when_no_pong_within_dead_threshold`).
        let cfg = worker_config(50, 400);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

        for n in 1..=16 {
            // Skip the `StatusRequest` (Text) frames the worker also emits; we
            // Pong each PING to keep the dead-peer detector satisfied.
            let payload = loop {
                let msg =
                    tokio::time::timeout(std::time::Duration::from_secs(2), from_worker.recv())
                        .await
                        .unwrap_or_else(|_| panic!("expected PING #{n} within 2s"))
                        .expect("from_worker channel must remain open while worker runs");
                match msg {
                    ws::Message::Ping(p) => break p,
                    ws::Message::Text(_) => continue,
                    other => panic!("expected Ping or StatusRequest #{n}, got {other:?}"),
                }
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
    // These pin down what each inbound frame type does at the worker layer:
    //   * Ping → tolerated (production: tungstenite auto-pongs at the framing
    //            layer; the worker code itself only needs to not panic).
    //   * Pong → consumed by the dead-peer detector; loop continues.
    //   * Binary → log + continue (out-of-protocol).
    //   * Text → decoded as `SupervisorToSwitchboard`; an unparseable payload is
    //            logged and the loop continues (a `ProtocolError` would close).
    // In all these cases the worker must keep running until we send Close.

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn inbound_ping_does_not_terminate_loop(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        // Wide ping cadence / dead-pong deadline so neither fires during this
        // test — we only care that an inbound Ping doesn't take down the loop.
        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

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
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

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
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

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
        let host_id = insert_host(&pool).await?;
        let (to_worker, _from_worker, socket) = scripted_socket();

        let cfg = worker_config(60_000, 600_000);
        let jh = tokio::spawn(SupervisorWSWorker::run(pool, host_id, socket, cfg, None));

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

    // -- reconciliation contract --------------------------------------------
    //
    // These pin down the reconciliation rows documented on
    // `SupervisorWSWorker::reconcile`. Each test builds the DB precondition
    // (`J_sb = hosts.current_job` and its `job_state`), seeds the worker's
    // `last_seen_status` cache with the reported supervisor status (`J_sup`),
    // invokes `reconcile`, and asserts the converged DB state plus any command
    // the worker must emit. `reconcile` is driven directly (rather than through
    // `run_loop`), mirroring how `with_txn` is unit-tested above; the cache it
    // reads is what `run_loop` would otherwise populate from a `StatusResponse`.

    /// Case 1: switchboard has no assigned job and the supervisor is idle. The
    /// two views already agree, so reconcile is a no-op: nothing is assigned and
    /// no command is sent.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case1_idle_and_unassigned_is_noop(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("case 1: reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, host_id).await?,
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
        let host_id = insert_host(&pool).await?;
        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        let zombie = Uuid::new_v4();
        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id: zombie,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("case 2: reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, host_id).await?,
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
    /// the job was lost. The worker finalizes it as `host_dropped_job` and
    /// clears the assignment. (Restart policy is 0 here, so no successor.)
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case3_lost_job_is_finalized(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("case 3: reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "case 3: lost job must be finalized"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
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
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        // Bound but not yet started; the supervisor reports it is now running.
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("case 4: reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "ready",
            "case 4: DB must adopt the reported execution state"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
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
    /// `host_dropped_job`) and `StopJob`s the job to acknowledge the terminal
    /// report, but **keeps** the assignment — the supervisor still retains the
    /// terminal record. A follow-up pass that observes `Idle` releases the
    /// pointer. The task outcome the supervisor declared out-of-band before
    /// termination must be preserved across the finalize.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case4_terminated_finalizes_and_acks(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

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

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Terminated,
        });
        worker
            .reconcile()
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
            "case 4 (terminated): must finalize as workload_exited, not host_dropped_job"
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
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "case 4 (terminated): assignment is kept until the host reports the job gone"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("case 4 (terminated): a StopJob ack should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("case 4 (terminated): expected StopJob, got {other:?}"),
        }

        // The supervisor acks and drops its retained record, reporting `Idle` on
        // the next status round trip. Reconcile now releases the assignment.
        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("case 4 (terminated): follow-up reconcile should succeed");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "case 4 (terminated): the assignment is released once the host reports Idle"
        );
        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "case 4 (terminated): the follow-up must not disturb the terminal row"
        );
        Ok(())
    }

    /// A supervisor may declare the task outcome while it is assigned the job,
    /// independently of the job's lifecycle state.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn apply_task_outcome_while_assigned_persists(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

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
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

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
    /// host is a no-op (returns `false`) and writes nothing: the setter is
    /// guarded on this host's assignment pointer.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn apply_task_outcome_not_assigned_is_noop(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let other_host = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        // The job is dispatched to `other_host`, not to our worker.
        let job_id = insert_job(&pool, token_id, other_host, "ready", 0).await?;
        set_current_job(&pool, other_host, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        assert!(
            !worker
                .apply_task_outcome(job_id, TaskExitStatus::Success, Some("nope".to_string()))
                .await?,
            "setting the outcome on a job assigned elsewhere must be a no-op"
        );
        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (None, None),
            "no outcome must be written for a job assigned to another host"
        );
        Ok(())
    }

    /// Case 5: switchboard and supervisor disagree on the job id. The assigned
    /// job is lost (finalize as `host_dropped_job`, clear assignment) while
    /// the reported job is an unassigned zombie that must be `StopJob`'d.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_case5_mismatched_job_finalizes_and_stops(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let assigned = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(assigned)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        let reported = Uuid::new_v4();
        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id: reported,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("case 5: reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, assigned).await?,
            "finalized",
            "case 5: the assigned-but-lost job must be finalized"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
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

    // -- finalized-but-still-assigned (sticky-host recovery) ----------------
    //
    // A job can be `finalized` while `hosts.current_job` still points at it (the
    // job finalized out-of-band but the host never reported the `Terminated`
    // that releases the assignment). These pin down the `finalized` rows of the
    // convergence table: reconcile must release the pointer only once the report
    // confirms the supervisor no longer holds the job, must never re-finalize or
    // un-finalize the terminal row, and must never enqueue a restart successor.

    /// Finalized + the supervisor reports `Idle`: the host has released the job,
    /// so reconcile clears the assignment. The terminal row is untouched (no
    /// re-finalize, no restart successor) and no command is emitted.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_finalized_idle_releases_assignment(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_finalized_assigned_job(&pool, token_id, host_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("finalized + idle reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "finalized + idle: the stuck assignment must be released"
        );
        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "finalized + idle: the terminal row must stay finalized"
        );
        assert_eq!(
            termination_reason_of(&pool, job_id).await?.as_deref(),
            Some("workload_exited"),
            "finalized + idle: the original termination reason must be preserved"
        );
        let job_count: i64 = sqlx::query_scalar("select count(*) from tml_switchboard.jobs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            job_count, 1,
            "finalized + idle: releasing a terminal job must not enqueue a restart"
        );
        assert!(
            from_worker.try_recv().is_err(),
            "finalized + idle: no command should be emitted"
        );
        Ok(())
    }

    /// Finalized + the supervisor still reports *this* job in a *running* state:
    /// this is the un-finalize hole. Reconcile must NOT adopt the running state
    /// (which would rewrite `job_state` back to a live value); it must keep the
    /// row finalized, retain the assignment, and `StopJob`-ack to drive teardown.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_finalized_same_running_does_not_unfinalize(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_finalized_assigned_job(&pool, token_id, host_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        // The supervisor reports our finalized job as still *running*.
        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("finalized + same-running reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "finalized + same-running: the terminal row must NOT be un-finalized"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "finalized + same-running: keep the assignment until the host reports gone"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("finalized + same-running: a StopJob ack should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("finalized + same-running: expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// Finalized + the supervisor reports *this* job as a retained `Terminated`
    /// record: `StopJob`-ack so the supervisor drops it, but keep the assignment
    /// until it reports gone. The terminal row is untouched.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_finalized_same_terminated_acks_keeps_assignment(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_finalized_assigned_job(&pool, token_id, host_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Terminated,
        });
        worker
            .reconcile()
            .await
            .expect("finalized + same-terminated reconcile should succeed");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "finalized + same-terminated: keep the assignment until the host reports gone"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("finalized + same-terminated: a StopJob ack should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("finalized + same-terminated: expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// Finalized + the supervisor reports a *different* job: ours is gone from the
    /// host, so release our pointer; the reported job is an unassigned zombie to
    /// `StopJob`. The terminal row is untouched.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_finalized_foreign_releases_and_stops(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_finalized_assigned_job(&pool, token_id, host_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        let foreign = Uuid::new_v4();
        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id: foreign,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("finalized + foreign reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "finalized + foreign: our gone job's assignment must be released"
        );
        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "finalized + foreign: the terminal row must stay finalized"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("finalized + foreign: a StopJob for the foreign zombie should be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, foreign),
            other => panic!("finalized + foreign: expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// End-to-end sticky-host recovery: an `Error` event finalizes the job but
    /// (by protocol) leaves the assignment in place awaiting a `Terminated` that
    /// never comes. A later reconcile that observes the supervisor `Idle` must
    /// release the now-stuck assignment — the exact bug this fix closes.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_recovers_sticky_host_after_error_without_terminated(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        // The supervisor reports a fatal job error: the job finalizes but the
        // assignment is deliberately kept (awaiting a `Terminated` follow-up).
        worker
            .handle_supervisor_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::Error {
                    error: JobError {
                        error_kind: JobErrorKind::ImageNotFound,
                        description: "manifest missing".to_string(),
                    },
                },
            })
            .await
            .expect("error event should be handled");
        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "an Error finalize keeps the assignment (awaiting Terminated)"
        );

        // The host never sends `Terminated`; it simply reports `Idle` on the next
        // status round trip. Reconcile must release the stuck assignment.
        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("recovery reconcile should succeed");

        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "reconcile must release the assignment a missing Terminated left stuck"
        );
        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "finalized",
            "recovery must not disturb the already-terminal row"
        );
        let job_count: i64 = sqlx::query_scalar("select count(*) from tml_switchboard.jobs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            job_count, 1,
            "recovery must not enqueue a restart successor"
        );
        assert!(
            from_worker.try_recv().is_err(),
            "recovery on an Idle report emits no command"
        );
        Ok(())
    }

    /// Give `job_id`'s requested image a single registry location and pin it as
    /// the job's `resolved_image_id` — the precondition the scheduler
    /// establishes before a job is dispatched. Returns the resolved digest and
    /// the location's `(registry, repository)` so the caller can assert the
    /// built `StartJob` carries them.
    async fn register_resolved_image(
        pool: &PgPool,
        job_id: Uuid,
    ) -> anyhow::Result<(treadmill_rs::image::Digest, String, String)> {
        let image_id: Uuid =
            sqlx::query_scalar("select image_id from tml_switchboard.jobs where job_id = $1")
                .bind(job_id)
                .fetch_one(pool)
                .await?;
        let (registry, repository) = ("registry.example".to_string(), "team/image".to_string());
        sql::image::upsert_location(pool, image_id, &registry, &repository, "canonical").await?;
        let digest_str: String =
            sqlx::query_scalar("select manifest_digest from tml_switchboard.images where id = $1")
                .bind(image_id)
                .fetch_one(pool)
                .await?;
        let digest: treadmill_rs::image::Digest = digest_str.parse().expect("valid digest");
        sql::job::set_resolved_image(job_id, image_id, pool).await?;
        Ok((digest, registry, repository))
    }

    /// Scheduled + the supervisor is idle: the job hasn't been picked up yet, so
    /// the worker dispatches `StartJob`. The DB stays `scheduled` (the send is
    /// idempotent; a later pass adopts the reported running state), and the
    /// emitted message carries the scheduler-resolved image + its locations.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_scheduled_idle_dispatches_start_job(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        let (digest, registry, repository) = register_resolved_image(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("scheduled + idle reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "scheduled",
            "scheduled + idle: StartJob must not transition the DB state"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "scheduled + idle: the assignment must be retained"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("scheduled + idle: a StartJob must be emitted"),
        ) {
            SwitchboardToSupervisor::StartJob(m) => {
                assert_eq!(m.job_id, job_id, "StartJob must target the scheduled job");
                match m.image_spec {
                    ImageSpecification::Image {
                        manifest_digest,
                        locations,
                    } => {
                        assert_eq!(
                            manifest_digest, digest,
                            "StartJob must carry the scheduler-resolved digest"
                        );
                        assert_eq!(locations.len(), 1, "exactly one location was registered");
                        assert_eq!(locations[0].registry, registry);
                        assert_eq!(locations[0].repository, repository);
                    }
                    other => panic!("expected a concrete Image spec, got {other:?}"),
                }
                assert!(m.ssh_keys.is_empty(), "insert_job seeds no ssh keys");
                assert!(m.parameters.is_empty(), "insert_job seeds no parameters");
            }
            other => panic!("scheduled + idle: expected StartJob, got {other:?}"),
        }
        Ok(())
    }

    /// Scheduled, but the supervisor is busy with some *other* job: that job is a
    /// zombie to stop, while the scheduled job is left untouched for the next
    /// pass to start (no StartJob this pass, no DB transition).
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_scheduled_foreign_zombie_keeps_scheduled(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        let zombie = Uuid::new_v4();
        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id: zombie,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("scheduled + foreign zombie reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "scheduled",
            "the scheduled job must be left scheduled for the next pass"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "the assignment must be retained"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("a StopJob for the foreign zombie must be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, zombie),
            other => panic!("expected StopJob for the zombie, got {other:?}"),
        }
        Ok(())
    }

    /// `build_start_job_message` maps a concrete-image job to its resolved digest
    /// + locations, and a resume job to `ResumeJob` (carrying the original id).
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn build_start_job_message_concrete_and_resume(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let mut conn = pool.acquire().await?;

        // Concrete image: resolved digest + its locations.
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        let (digest, _registry, _repository) = register_resolved_image(&pool, job_id).await?;
        let job = sql::job::fetch_by_job_id(job_id, &pool).await?;
        let msg = sql::job::build_start_job_message(&job, &mut conn, None)
            .await
            .expect("concrete-image StartJob should build");
        assert_eq!(msg.job_id, job_id);
        match msg.image_spec {
            ImageSpecification::Image {
                manifest_digest,
                locations,
            } => {
                assert_eq!(manifest_digest, digest);
                assert_eq!(locations.len(), 1);
            }
            other => panic!("expected concrete Image spec, got {other:?}"),
        }

        // Resume job: `ResumeJob` carrying the original job's id. A resume row has
        // no image/group reference (the `valid_init_spec` invariant), so it is
        // inserted directly rather than via `insert_job`. `resume_job_id` is an FK,
        // so it must point at a real job — reuse the concrete one above.
        let resume_target = job_id;
        let resume_job = Uuid::new_v4();
        sqlx::query(
            "insert into tml_switchboard.jobs \
             (job_id, resume_job_id, restart_job_id, image_id, image_group_id, \
              image_group_generation, ssh_keys, \
              restart_policy, enqueued_by_token_id, host_tag_requirements, job_timeout, job_state, \
              initializing_stage, queued_at, started_at, dispatched_on_host_id, ssh_endpoints, \
              termination_reason, task_exit_status, exit_message, terminated_at, last_updated_at) \
             values \
             ($1, $2, null, null, null, null, '{}'::text[], row(0)::tml_switchboard.restart_policy, \
              $3, '{}'::text[], interval '1 hour', 'queued', null, now(), null, null, null, \
              null, null, null, null, default)",
        )
        .bind(resume_job)
        .bind(resume_target)
        .bind(token_id)
        .execute(&pool)
        .await?;
        let rjob = sql::job::fetch_by_job_id(resume_job, &pool).await?;
        let rmsg = sql::job::build_start_job_message(&rjob, &mut conn, None)
            .await
            .expect("resume StartJob should build");
        match rmsg.image_spec {
            ImageSpecification::ResumeJob { job_id } => assert_eq!(
                job_id, resume_target,
                "ResumeJob must carry the original job's id"
            ),
            other => panic!("expected ResumeJob spec, got {other:?}"),
        }
        Ok(())
    }

    /// With log streaming enabled, `build_start_job_message` populates
    /// `log_streaming` with this job's subject prefix and a freshly minted
    /// **bearer** write token scoped to publish only `logs.<job-id>.>`. Decodes
    /// the JWT claims directly — no live NATS needed (token minting is pure).
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn build_start_job_message_populates_scoped_write_token(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        use base64::Engine as _;

        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let mut conn = pool.acquire().await?;

        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        register_resolved_image(&pool, job_id).await?;
        let job = sql::job::fetch_by_job_id(job_id, &pool).await?;

        // A throwaway account seed: doubles as the JWT signing key.
        let account = nats_jwt::KeyPair::new_account();
        let ls_config = crate::config::LogStreamingConfig {
            nats_url: "nats://nats.example:4222".to_string(),
            jetstream_domain: None,
            account_seed: account.seed().expect("account seed"),
        };

        let msg = sql::job::build_start_job_message(&job, &mut conn, Some(&ls_config))
            .await
            .expect("StartJob should build with log streaming");

        let dispatch = msg
            .log_streaming
            .expect("log_streaming must be populated when enabled");
        assert_eq!(dispatch.nats_url, ls_config.nats_url);
        assert_eq!(
            dispatch.subject_prefix,
            crate::log_streaming::subject_prefix(job_id)
        );

        // Decode (unverified) the write token's claims as JSON and check the pub
        // scope. Parsed as JSON rather than `nats_jwt::Claims` because that type
        // cannot round-trip its own output (its permission lists skip-serialize
        // when empty but have no serde default).
        let payload = dispatch
            .write_token
            .split('.')
            .nth(1)
            .expect("jwt has a payload segment");
        let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("jwt payload is base64url");
        let claims: serde_json::Value =
            serde_json::from_slice(&claims_bytes).expect("payload is JSON");

        assert_eq!(claims["nats"]["type"], "user");
        assert_eq!(
            claims["nats"]["bearer_token"], true,
            "write token must be a bearer token"
        );
        assert_eq!(
            claims["nats"]["pub"]["allow"],
            serde_json::json!([format!("logs.{job_id}.>")]),
            "write token must publish-scope exactly this job's subjects"
        );
        assert!(
            claims["nats"].get("sub").is_none(),
            "write token must not grant subscribe scope"
        );

        // Disabled (None) leaves the field unset.
        let plain = sql::job::build_start_job_message(&job, &mut conn, None)
            .await
            .expect("StartJob should build without log streaming");
        assert!(plain.log_streaming.is_none());

        Ok(())
    }

    // -- asynchronous event path (SupervisorEvent) --------------------------
    //
    // These pin down `handle_supervisor_event`, the out-of-band mirror of the
    // supervisor's job events into the DB (and into the status cache). Each test
    // builds the DB precondition, drives one event through the worker, and
    // asserts the DB transition, any emitted command, and the refreshed cache.

    /// A `StateTransition` event adopts the reported running state through the
    /// same `apply_running_state` reconcile uses, and refreshes the status cache.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn event_state_transition_adopts_and_caches(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker
            .handle_supervisor_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: RunningJobState::Ready,
                    status_message: None,
                },
            })
            .await
            .expect("state transition event should be handled");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "ready",
            "the event must adopt the reported running state"
        );
        match worker.last_seen_status {
            Some(ReportedSupervisorStatus::OngoingJob {
                job_id: cached,
                job_state: RunningJobState::Ready,
            }) => assert_eq!(cached, job_id, "the cache must reflect the new state"),
            other => panic!("expected cached OngoingJob(Ready), got {other:?}"),
        }
        assert!(
            from_worker.try_recv().is_err(),
            "a non-terminal transition emits no command"
        );
        Ok(())
    }

    /// A `StateTransition` to `Terminated` finalizes the job (workload_exited)
    /// and `StopJob`-acks, but keeps the assignment and caches the reported
    /// terminal state (not `Idle`): the supervisor still retains the record, so
    /// the pointer is released only later, by a reconcile that observes `Idle`.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn event_state_transition_terminated_finalizes_and_acks(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker
            .handle_supervisor_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: RunningJobState::Terminated,
                    status_message: None,
                },
            })
            .await
            .expect("terminated transition event should be handled");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "a terminated event keeps the assignment until the host reports the job gone"
        );
        match worker.last_seen_status {
            Some(ReportedSupervisorStatus::OngoingJob {
                job_id: cached,
                job_state: RunningJobState::Terminated,
            }) => assert_eq!(
                cached, job_id,
                "the cache must reflect the reported terminal state, not Idle"
            ),
            other => panic!("expected cached OngoingJob(Terminated), got {other:?}"),
        }
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("a StopJob ack must be emitted"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("expected StopJob ack, got {other:?}"),
        }

        // The host acks and drops the retained record, reporting `Idle`; a
        // reconcile then releases the assignment (the sticky-host recovery path).
        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("follow-up reconcile should succeed");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "the assignment is released once the host reports Idle"
        );
        Ok(())
    }

    /// A `DeclareExitStatus` event records the task outcome via the same path as
    /// the direct setter.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn event_declare_exit_status_records_outcome(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker
            .handle_supervisor_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::DeclareExitStatus {
                    outcome: TaskExitStatus::Success,
                    message: Some("all good".to_string()),
                },
            })
            .await
            .expect("declare-exit-status event should be handled");

        assert_eq!(
            task_outcome_of(&pool, job_id).await?,
            (Some("success".to_string()), Some("all good".to_string())),
            "the event must record the declared outcome"
        );
        Ok(())
    }

    /// An `Error` event finalizes the job with the mapped termination reason and
    /// records the error description, clearing the assignment and caching `Idle`.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn event_error_finalizes_with_mapped_reason(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker
            .handle_supervisor_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::Error {
                    error: JobError {
                        error_kind: JobErrorKind::ImageNotFound,
                        description: "manifest missing".to_string(),
                    },
                },
            })
            .await
            .expect("error event should be handled");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        let reason: Option<String> = sqlx::query_scalar(
            "select termination_reason::text from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            reason.as_deref(),
            Some("image_error"),
            "ImageNotFound must map to image_error"
        );
        let (_outcome, message) = task_outcome_of(&pool, job_id).await?;
        assert_eq!(
            message.as_deref(),
            Some("manifest missing"),
            "the error description must be recorded as exit_message"
        );

        // Error finalization does not release the assignment — the supervisor
        // may still hold the job; reconcile releases it once the supervisor
        // reports it gone (the sticky-host recovery path).
        assert_eq!(current_job_of(&pool, host_id).await?, Some(job_id));
        // The cache is NOT forced to `Idle`: an `Error` is no confirmation the
        // supervisor dropped the job, so we leave the last reported status (here
        // never set) untouched rather than assuming the job is gone.
        assert!(
            worker.last_seen_status.is_none(),
            "an Error event must not synthesize an Idle status"
        );
        Ok(())
    }

    /// An event for a job that is not the one assigned to this host is dropped:
    /// no DB change, no cache update, no command.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn event_for_unassigned_job_is_dropped(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        // The job exists and is bound to the host row, but is NOT the host's
        // current_job (nothing is assigned), so the event must be dropped.
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker
            .handle_supervisor_event(SupervisorEvent::JobEvent {
                job_id,
                event: SupervisorJobEvent::StateTransition {
                    new_state: RunningJobState::Ready,
                    status_message: None,
                },
            })
            .await
            .expect("event for an unassigned job should be a no-op, not an error");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "scheduled",
            "an event for an unassigned job must not mutate it"
        );
        assert!(
            worker.last_seen_status.is_none(),
            "a dropped event must not touch the status cache"
        );
        assert!(
            from_worker.try_recv().is_err(),
            "a dropped event emits no command"
        );
        Ok(())
    }

    // -- stop pre-check: execution timeout & user cancel --------------------
    //
    // These pin down reconcile's "should this assigned job stop?" pre-check,
    // shared by execution-timeout and user-cancel. Both are re-derived from
    // fresh DB state each pass (see `SqlJob::switchboard_stop_reason`).

    /// Push a running job's `started_at` two hours into the past so it is over
    /// its one-hour `job_timeout` (the value `insert_job` seeds).
    async fn expire_started_at(pool: &PgPool, job_id: Uuid) -> anyhow::Result<()> {
        sqlx::query(
            "update tml_switchboard.jobs \
             set started_at = now() - interval '2 hours' where job_id = $1",
        )
        .bind(job_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Set the DB-side user-cancel signal (`cancel_requested_at`).
    async fn request_cancel(pool: &PgPool, job_id: Uuid) -> anyhow::Result<()> {
        sqlx::query(
            "update tml_switchboard.jobs set cancel_requested_at = now() where job_id = $1",
        )
        .bind(job_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Read a job's `termination_reason` as its enum text.
    async fn termination_reason_of(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "select termination_reason::text from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?)
    }

    /// Past its deadline while the supervisor still reports it running: the
    /// worker (re)issues StopJob and keeps tracking the job (no finalize yet).
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_execution_timeout_running_stops(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        expire_started_at(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("timeout (running) reconcile should succeed");

        assert_eq!(
            job_state_of(&pool, job_id).await?,
            "ready",
            "the job keeps being tracked until the supervisor reports it gone"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "the assignment is retained while teardown is in flight"
        );
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("a StopJob must be issued for the timed-out job"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// Past its deadline and the supervisor reports it `Terminated` (a retained
    /// terminal record): finalize as `execution_timeout` and `StopJob`-ack, but
    /// **keep** the assignment — the supervisor still holds the record. A
    /// follow-up pass that observes `Idle` releases the pointer.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_execution_timeout_terminated_finalizes(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "terminating", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        expire_started_at(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Terminated,
        });
        worker
            .reconcile()
            .await
            .expect("timeout (terminated) reconcile should succeed");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            termination_reason_of(&pool, job_id).await?.as_deref(),
            Some("execution_timeout"),
            "a timed-out job must finalize as execution_timeout"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "the assignment is kept while the supervisor still reports the job"
        );
        match decode_outbound(from_worker.try_recv().expect("a StopJob ack is expected")) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("expected StopJob ack, got {other:?}"),
        }

        // The supervisor drops the record and reports `Idle`; reconcile releases.
        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("timeout (terminated) follow-up reconcile should succeed");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "the assignment is released once the supervisor reports Idle"
        );
        assert_eq!(
            termination_reason_of(&pool, job_id).await?.as_deref(),
            Some("execution_timeout"),
            "the follow-up must not change the recorded termination reason"
        );
        Ok(())
    }

    /// Within its deadline: the pre-check does not fire — the job is adopted
    /// normally and no StopJob is issued. This is the re-check that lets a
    /// deadline extension (a larger `job_timeout` / later `started_at`) rescue a
    /// job: the very next pass simply sees it is no longer expired.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_within_deadline_not_stopped(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        // insert_job seeds started_at = now() with a one-hour timeout: not
        // expired.
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("within-deadline reconcile should succeed");

        assert_eq!(job_state_of(&pool, job_id).await?, "ready");
        assert_eq!(current_job_of(&pool, host_id).await?, Some(job_id));
        assert!(
            from_worker.try_recv().is_err(),
            "a within-deadline job must not be stopped"
        );
        Ok(())
    }

    /// User-cancel while the supervisor still reports the job running: (re)issue
    /// StopJob, keep tracking it.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_user_cancel_running_stops(pool: PgPool) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        request_cancel(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Ready,
        });
        worker
            .reconcile()
            .await
            .expect("user-cancel (running) reconcile should succeed");

        assert_eq!(current_job_of(&pool, host_id).await?, Some(job_id));
        match decode_outbound(
            from_worker
                .try_recv()
                .expect("a StopJob must be issued for the canceled job"),
        ) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("expected StopJob, got {other:?}"),
        }
        Ok(())
    }

    /// User-cancel and the supervisor reports the job `Terminated` (a retained
    /// terminal record): finalize as `user_canceled` and `StopJob`-ack, but keep
    /// the assignment until the supervisor reports the job gone — the column must
    /// not be cleared while the supervisor still holds the record.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_user_cancel_terminated_keeps_then_releases(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "terminating", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        request_cancel(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::OngoingJob {
            job_id,
            job_state: RunningJobState::Terminated,
        });
        worker
            .reconcile()
            .await
            .expect("user-cancel (terminated) reconcile should succeed");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            termination_reason_of(&pool, job_id).await?.as_deref(),
            Some("user_canceled"),
            "a canceled job must finalize as user_canceled"
        );
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            Some(job_id),
            "the assignment is kept while the supervisor still reports the job"
        );
        match decode_outbound(from_worker.try_recv().expect("a StopJob ack is expected")) {
            SwitchboardToSupervisor::StopJob(m) => assert_eq!(m.job_id, job_id),
            other => panic!("expected StopJob ack, got {other:?}"),
        }

        // The supervisor drops the record and reports `Idle`; reconcile releases.
        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("user-cancel (terminated) follow-up reconcile should succeed");
        assert_eq!(
            current_job_of(&pool, host_id).await?,
            None,
            "the assignment is released once the supervisor reports Idle"
        );
        Ok(())
    }

    /// User-cancel and the supervisor reports `Idle` (job already gone): finalize
    /// as `user_canceled`.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_user_cancel_idle_finalizes_user_canceled(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "ready", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        request_cancel(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, _from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("user-cancel (idle) reconcile should succeed");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            termination_reason_of(&pool, job_id).await?.as_deref(),
            Some("user_canceled"),
            "a canceled job must finalize as user_canceled"
        );
        assert_eq!(current_job_of(&pool, host_id).await?, None);
        Ok(())
    }

    /// User-cancel of a still-`scheduled` job (never started): finalize as
    /// `user_canceled` instead of dispatching it — no StartJob is emitted.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reconcile_user_cancel_scheduled_finalizes_without_start(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let host_id = insert_host(&pool).await?;
        let user_id = insert_user(&pool).await?;
        let token_id = insert_token(&pool, user_id).await?;
        let job_id = insert_job(&pool, token_id, host_id, "scheduled", 0).await?;
        set_current_job(&pool, host_id, Some(job_id)).await?;
        request_cancel(&pool, job_id).await?;
        // A resolved image is registered so that, absent the cancel, this job
        // *would* be dispatched — proving the cancel is what suppresses StartJob.
        register_resolved_image(&pool, job_id).await?;

        let wiid = WorkerCtx::obtain_worker_instance_id(&pool, host_id).await?;
        let (_to_worker, mut from_worker, mut worker) =
            scripted_worker(pool.clone(), host_id, wiid, worker_config(50, 250));

        worker.last_seen_status = Some(ReportedSupervisorStatus::Idle);
        worker
            .reconcile()
            .await
            .expect("user-cancel (scheduled) reconcile should succeed");

        assert_eq!(job_state_of(&pool, job_id).await?, "finalized");
        assert_eq!(
            termination_reason_of(&pool, job_id).await?.as_deref(),
            Some("user_canceled"),
            "a canceled scheduled job must finalize as user_canceled"
        );
        assert_eq!(current_job_of(&pool, host_id).await?, None);
        assert!(
            from_worker.try_recv().is_err(),
            "a canceled scheduled job must not be dispatched"
        );
        Ok(())
    }
}
