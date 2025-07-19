use anyhow::{Context, Result};
use axum::extract::ws::WebSocket;
use sqlx::{PgExecutor, Postgres, Transaction};
use uuid::Uuid;

use crate::serve::AppState;
use crate::sql;

pub struct SupervisorWSWorker {
    app_state: AppState,
    supervisor_id: Uuid,
    socket: WebSocket,
    worker_instance_id: u64,
}

impl SupervisorWSWorker {
    #[tracing::instrument(skip(app_state))]
    pub async fn run(app_state: AppState, supervisor_id: Uuid, mut socket: WebSocket) {
        if let Err(e) = Self::run_inner(app_state, supervisor_id, socket).await {
            tracing::error!("SupervisorWSWorker::run terminated with error: {e:?}");
        } else {
            tracing::info!("SupervisorWSWorker::run terminated successfully.");
        }
    }

    pub async fn run_inner(
        app_state: AppState,
        supervisor_id: Uuid,
        mut socket: WebSocket,
    ) -> Result<()> {
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
        // obtains a unique worker instance ID, and then runs any database
        // operation in a transaction to ensure that this worker instance ID is
        // still current. It also periodically checks that its ID is still
        // current. If there is a mismatch between the local worker ID and the
        // supervisor's database state, the worker terminates. This prevents
        // concurrent workers for the same supervisor to perform any conflicting
        // operations.

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

    async fn worker_instance_id_check_txn<R, F: FnOnce(&mut Transaction<'_, Postgres>) -> R>(
        &self,
        txn_fn: F,
    ) -> Result<R> {
        let mut txn = self.app_state.pool().begin().await.with_context(|| {
            format!(
                "Creating SupervisorWSWorker transaction for supervisor {:?} \
		 with worker_instance_id {}",
                self.supervisor_id, self.worker_instance_id
            )
        })?;

	// First, allow the user to perform their own statements in
	// the transaction:
        let res = txn_fn(&mut txn);

	// In Postgres' default read-committed transaction isolation
	// model, we may observe writes by committed transactions in
	// SELECT statements of the current transaction. Thus, at the
	// very end of this transaction, check that the
	// `worker_instance_id` still matches:
	let current_worker_instance_id: u64 =
	    sql::supervisor::get_current_worker_instance_id(self.supervisor_id, &mut *txn)
	    .await?
	    .try_into()
	    .expect(
		"Database invariant violated: worker_instance_id must be zero \
		 or positive integer!",
            );

	if current_worker_instance_id == self.worker_instance_id {
            txn.commit().await.with_context(|| {
		format!(
                    "Committing SupervisorWSWorker transaction for supervisor \
		     {:?} with worker_instance_id {}",
                    self.supervisor_id, self.worker_instance_id
		)
            })?;
	} else {
	    todo!()
	}

        Ok(res)
    }

    async fn run_loop(self) -> Result<()> {
        todo!()
    }
}
