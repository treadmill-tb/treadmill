use anyhow::{Context, Result};
use axum::extract::ws;
use futures_util::{Sink, Stream};
use sqlx::{PgPool, Postgres, Transaction};
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

pub struct SupervisorWSWorker<S: SupervisorSocket> {
    pool: PgPool,
    supervisor_id: Uuid,
    socket: S,
    worker_instance_id: u64,
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
    pub async fn run(pool: PgPool, supervisor_id: Uuid, socket: S) {
        match Self::run_inner(pool, supervisor_id, socket).await {
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

    pub async fn run_inner(pool: PgPool, supervisor_id: Uuid, socket: S) -> WorkerResult<()> {
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

    async fn run_loop(self) -> WorkerResult<()> {
        todo!()
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
    ) -> SupervisorWSWorker<NoSocket> {
        SupervisorWSWorker {
            pool,
            supervisor_id,
            socket: NoSocket,
            worker_instance_id,
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
        let worker = worker(pool.clone(), supervisor_id, worker_instance_id);

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

        let worker = worker(pool.clone(), supervisor_id, our_id);
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
        let worker = worker(pool.clone(), supervisor_id, worker_instance_id);

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
            SupervisorWSWorker::run(pool, supervisor_id, socket),
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
            SupervisorWSWorker::run(pool, supervisor_id, socket),
        )
        .await
        .expect("worker should return promptly on socket stream end");

        Ok(())
    }
}
