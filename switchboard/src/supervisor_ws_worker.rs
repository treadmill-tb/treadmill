use anyhow::{Context, Result};
use axum::extract::ws::WebSocket;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::serve::AppState;
use crate::sql;

pub struct SupervisorWSWorker {
    app_state: AppState,
    supervisor_id: Uuid,
    socket: WebSocket,
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

impl SupervisorWSWorker {
    #[tracing::instrument(skip(app_state))]
    pub async fn run(app_state: AppState, supervisor_id: Uuid, socket: WebSocket) {
        match Self::run_inner(app_state, supervisor_id, socket).await {
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
        app_state: AppState,
        supervisor_id: Uuid,
        socket: WebSocket,
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
        let worker_instance_id = Self::obtain_worker_instance_id(&app_state, supervisor_id).await?;

        // Now, using this ID, construct the worker instance:
        let worker = SupervisorWSWorker {
            app_state,
            supervisor_id,
            socket,
            worker_instance_id,
        };

        worker.run_loop().await
    }

    async fn obtain_worker_instance_id(app_state: &AppState, supervisor_id: Uuid) -> Result<u64> {
        let db_worker_instance_id: i64 =
            sql::supervisor::increment_worker_instance_id(supervisor_id, app_state.pool())
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
        let mut txn = self.app_state.pool().begin().await.with_context(|| {
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
