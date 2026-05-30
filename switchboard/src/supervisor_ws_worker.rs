use anyhow::{Context, Result};
use axum::extract::ws;
use futures_util::{Sink, Stream, StreamExt};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::{Duration, Interval, interval};
use treadmill_rs::api::switchboard_supervisor::SocketConfig;
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

pub struct SupervisorWSWorker<S: SupervisorSocket> {
    pool: PgPool,
    supervisor_id: Uuid,
    socket: S,
    worker_instance_id: u64,
    config: SupervisorWSWorkerConfig,
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
        let worker_instance_id = Self::obtain_worker_instance_id(&pool, supervisor_id).await?;

        // Now, using this ID, construct the worker instance:
        let worker = SupervisorWSWorker {
            pool,
            supervisor_id,
            socket,
            worker_instance_id,
            config,
        };

        worker.run_loop().await
    }

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

    async fn handle_supervisor_msg(&mut self, msg: ws::Message) -> WorkerResult<bool> {
        match msg {
            ws::Message::Close(_close_frame) => {
                tracing::info!("SupervisorWSWorker: supervisor sent close message, terminating.");
                Ok(true)
            }
            _ => unimplemented!(),
        }
    }

    async fn run_loop(mut self) -> WorkerResult<()> {
        loop {
            tokio::select! {
                opt_msg = self.socket.next() => {
                    let Some(res_msg) = opt_msg else {
                        tracing::info!("SupervisorWSWorker socket closed, terminating.");
                        return Ok(());
                    };

                    let msg = res_msg.context("SupervisorWSWorker reading message from supervisor WebSocket")?;

                    if self.handle_supervisor_msg(msg).await? {
                        // We've been asked to terminate:
                        return Ok(());
                    }
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
            pool,
            supervisor_id,
            socket: NoSocket,
            worker_instance_id,
            config,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn obtain_worker_instance_id_starts_at_one_and_increments(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        for expected in 1u64..=3 {
            let got =
                SupervisorWSWorker::<NoSocket>::obtain_worker_instance_id(&pool, supervisor_id)
                    .await?;
            assert_eq!(got, expected);
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn with_txn_runs_closure_and_commits_when_current(pool: PgPool) -> anyhow::Result<()> {
        let supervisor_id = insert_supervisor(&pool).await?;
        let worker_instance_id =
            SupervisorWSWorker::<NoSocket>::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let worker = worker(
            pool.clone(),
            supervisor_id,
            worker_instance_id,
            worker_config(50, 250),
        );

        let renamed = "renamed-inside-txn".to_string();
        let returned = worker
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
        let our_id =
            SupervisorWSWorker::<NoSocket>::obtain_worker_instance_id(&pool, supervisor_id).await?;
        // Simulate a takeover: a newer worker bumps the instance ID.
        let newer_id =
            SupervisorWSWorker::<NoSocket>::obtain_worker_instance_id(&pool, supervisor_id).await?;
        assert_eq!(newer_id, our_id + 1);

        let worker = worker(pool.clone(), supervisor_id, our_id, worker_config(50, 250));
        let err = worker
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
        let worker_instance_id =
            SupervisorWSWorker::<NoSocket>::obtain_worker_instance_id(&pool, supervisor_id).await?;
        let worker = worker(
            pool.clone(),
            supervisor_id,
            worker_instance_id,
            worker_config(50, 250),
        );

        let result: WorkerResult<()> = worker
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
}
