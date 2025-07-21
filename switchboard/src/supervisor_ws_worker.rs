use anyhow::{Context, Result, anyhow};
use axum::extract::ws::{self, WebSocket};
use futures_util::future::OptionFuture;
use futures_util::{SinkExt, StreamExt};
use sqlx::{PgExecutor, Postgres, Transaction};
use uuid::Uuid;

use treadmill_rs::api;

use crate::serve::AppState;
use crate::sql::{self, supervisor::SupervisorId};

// TODO: randomize this interval, to avoid having many workers hit the database
// all at once.
const CHECK_ENQUEUED_JOBS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub struct SupervisorWSWorker {
    app_state: AppState,
    supervisor_id: SupervisorId,
    worker_instance_id: u64,

    socket_sink: tokio::sync::Mutex<futures_util::stream::SplitSink<WebSocket, ws::Message>>,
    socket_source: tokio::sync::Mutex<futures_util::stream::SplitStream<WebSocket>>,

    ping_interval: tokio::sync::Mutex<tokio::time::Interval>,
    last_received_pong: std::sync::Mutex<tokio::time::Instant>,
    keepalive_timeout: std::time::Duration,

    check_enqueued_jobs_interval: tokio::sync::Mutex<tokio::time::Interval>,
}

impl SupervisorWSWorker {
    #[tracing::instrument(
        name = "SupervisorWSWorker",
        fields(worker_instance_id),
        skip(app_state, socket)
    )]
    pub async fn run(app_state: AppState, supervisor_id: sql::supervisor::SupervisorId, socket: WebSocket) {
        if let Err(e) = Self::run_inner(app_state, supervisor_id, socket).await {
            tracing::warn!("SupervisorWSWorker::run terminated with error: {e:?}");
        } else {
            tracing::info!("SupervisorWSWorker::run terminated successfully.");
        }
    }

    pub async fn run_inner(
        app_state: AppState,
        supervisor_id: SupervisorId,
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

        // Shortcut to the socket config options:
        let socket_config = &app_state.config().service.socket;

        // Determine the ping interval and keepalive period, after which the
        // connection is presumed dead:
        let ping_interval_duration =
            socket_config.keepalive.ping_interval.to_std().expect(
                "Configured WebSocket PING interval is out of range for core's Duration type",
            );
        let keepalive_timeout = socket_config.keepalive.timeout.to_std().expect(
            "Configured WebSocket PONG keepalive period is out of range for core's Duration type",
        );
        let mut ping_interval = tokio::time::interval(ping_interval_duration);
        let mut last_received_pong = tokio::time::Instant::now();

        // This should never fail, as the supervisor has successfully
        // authenticated:
        let worker_instance_id = Self::obtain_worker_instance_id(&app_state, supervisor_id).await?;
        tracing::Span::current().record("worker_instance_id", worker_instance_id);
        tracing::debug!("Obtained worker instance ID");

        // Now, using this ID and the channels + handle of the
        // SupervisorWSStreamTask, construct the worker instance:
        let (socket_sink, socket_source) = socket.split();
        let mut worker = SupervisorWSWorker {
            app_state,
            supervisor_id,
            worker_instance_id,

            socket_sink: tokio::sync::Mutex::new(socket_sink),
            socket_source: tokio::sync::Mutex::new(socket_source),

            ping_interval: tokio::sync::Mutex::new(ping_interval),
            last_received_pong: std::sync::Mutex::new(last_received_pong),
            keepalive_timeout,

            check_enqueued_jobs_interval: tokio::sync::Mutex::new(tokio::time::interval(
                CHECK_ENQUEUED_JOBS_INTERVAL,
            )),
        };

        // First, force a re-sync of the database state with the current
        // supervisor state:
        worker.resync_status().await.into_result().map(|_| ())?;

        loop {
            match worker.loop_iter().await {
                ControlFlowResult::Continue(Ok(())) => {
                    // Success, simply proceed to the next iteration.
                }
                ControlFlowResult::Continue(Err(e)) => {
                    tracing::warn!(
                        "SupervisorWSWorker loop iteration reports error, able to continue: {e:?}",
                    );
                }
                ControlFlowResult::ShutdownSuccess => {
                    tracing::info!("SupervisorWSWorker: shutting down");
                    return Ok(());
                }
                ControlFlowResult::ShutdownError(e) => {
                    return Err(e);
                }
            }
        }
    }

    async fn obtain_worker_instance_id(app_state: &AppState, supervisor_id: SupervisorId) -> Result<u64> {
        let db_worker_instance_id: i64 =
            sql::supervisor::increment_worker_instance_id(supervisor_id, app_state.pool())
                .await
                .with_context(|| {
                    format!(
                        "Obtaining new worker instance ID for supervisor \
                         {supervisor_id:?}",
                    )
                })?;

        Ok(db_worker_instance_id.try_into().expect(
            "Database invariant violated: worker_instance_id must be zero or \
             positive integer!",
        ))
    }

    async fn worker_instance_id_check_txn<R>(
        &self,
        txn_fn: impl AsyncFnOnce(&mut Transaction<'_, Postgres>) -> ControlFlowResult<R>,
    ) -> ControlFlowResult<R> {
        let mut txn = self
            .app_state
            .pool()
            .begin()
            .await
            .with_context(|| {
                format!(
                    "Creating SupervisorWSWorker transaction for supervisor {:?} \
                     with worker_instance_id {}",
                    self.supervisor_id, self.worker_instance_id
                )
            })
            .shutdown_on_err()?;

        // Set the transaction to execute with a serializable isolation
        // level. By default, postgres is read-commited, which may be confusing:
        // for instance, one transaction may read the supervisor's status, and
        // _then_ the job's status. With read-committed, another concurrent
        // transaction could transition the job out of a valid state to be
        // scheduled on a supervisor, leading to inconsistent reads.
        sqlx::Executor::execute(&mut *txn, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;")
            .await
            .context("Setting SupervisorWSWorker transaction isolation level to SERIALIZABLE")
            .shutdown_on_err()?;

        // First, allow the user to perform their own statements in the
        // transaction. If any of these error, we always roll back the
        // transaction and then forward the error:
        let txn_fn_res = txn_fn(&mut txn).await;

        if txn_fn_res.is_err() {
            // Closure exited with error, immediately roll back the transaction:
            txn.rollback()
                .await
                .with_context(|| {
                    format!(
                        "Rolling back SupervisorWSWorker transaction for \
                         supervisor {:?} with worker_instance_id {} as closure \
                         returned error",
                        self.supervisor_id, self.worker_instance_id
                    )
                })
                .shutdown_on_err()?;

            txn_fn_res
        } else {
            // The closure did not return an error, attempt to commit!

            // We want to ensure that we're still the current worker instance,
            // so we compare our instance ID. As the transaction is
            // serializable, the exact location of this check doesn't matter.
            //
            // However, in Postgres' default read-committed transaction
            // isolation model, we may could writes by committed transactions in
            // SELECT statements of the current transaction. Thus, if we were
            // running at that transaction isolation level, it'd be crucial that
            // we run this check at the very end of this transaction:
            let current_worker_instance_id: u64 =
                sql::supervisor::get(self.supervisor_id, &mut *txn)
                    .await
                    .with_context(|| {
                        format!(
                            "Querying current worker_instance_id for supervisor {}",
                            self.supervisor_id
                        )
                    })
                    .shutdown_on_err()?
                    .worker_instance_id
                    .try_into()
                    .expect(
                        "Database invariant violated: worker_instance_id must be zero \
                     or positive integer!",
                    );

            if current_worker_instance_id == self.worker_instance_id {
                // We were still the current worker at the end of the transaction,
                // following all writes, commit:
                txn.commit()
                    .await
                    .with_context(|| {
                        format!(
                            "Committing SupervisorWSWorker transaction for supervisor \
                             {:?} with worker_instance_id {}",
                            self.supervisor_id, self.worker_instance_id
                        )
                    })
                    .continue_on_err()?;

                txn_fn_res
            } else {
                // We were no longer the current worker at the end of the
                // transaction, rollback:
                txn.rollback()
                    .await
                    .with_context(|| {
                        format!(
                            "Rolling back SupervisorWSWorker transaction for \
                             supervisor {:?} with worker_instance_id {}",
                            self.supervisor_id, self.worker_instance_id
                        )
                    })
                    .shutdown_on_err()?;

                // For now, we simply return an error. In the future, it might make
                // sense to distinguish between the case where a transaction fails
                // because the worker is no longer current, or because of some other
                // reason.
                Err(anyhow!(
                    "Transaction aborted, as SupervisorWSWorker is no longer \
                     current (self.worker_instance_id = {} vs. \
                     current_worker_instance_id = {})!",
                    self.worker_instance_id,
                    current_worker_instance_id
                ))
                .shutdown_on_err()
            }
        }
    }

    async fn send_ws_message(&self, message: ws::Message) -> Result<(), axum::Error> {
        self.socket_sink
            .try_lock()
            .expect(
                "Failed to acquire immediate lock on `socket_sink`, this must \
		 never happen. All calls to `socket_sink.send` must go through \
		 `send_ws_message`, and there must be no concurrent calls to \
		 it (e.g., through multiple `tokio::select!` branches.",
            )
            .send(message)
            .await
    }

    async fn send_message(&self, message: api::switchboard_supervisor::Message) -> Result<()> {
        let serialized = serde_json::to_string(&message)
            .expect("Failed to serialize api::switchboard_supervisor::Message");

        self.send_ws_message(ws::Message::Text(serialized))
            .await
            .with_context(|| {
                format!(
                    "Sending message to supervisor {}: {message:?}",
                    self.supervisor_id,
                )
            })?;

        tracing::debug!("Sent message to supervisor: {message:?}");

        Ok(())
    }

    async fn wait_for_ws_message<R>(
        &self,
        allow_ws_close: bool,
        filter: impl Fn(ws::Message) -> ControlFlowResult<Option<R>>,
    ) -> ControlFlowResult<Option<R>> {
        let mut socket_source = self.socket_source.try_lock()
	    .expect("Failed to acquire immediate lock on ping_timeout, reentrant call to `wait_for_ws_message`, perhaps in `filter` closure?");

        loop {
            match socket_source.next().await {
                Some(Err(e)) => {
                    return ControlFlowResult::ShutdownError(anyhow::Error::new(e).context(
                        format!(
                            "Error while waiting for WebSocket message from \
                             supervisor {}",
                            self.supervisor_id,
                        ),
                    ));
                }

                Some(Ok(ws::Message::Ping(bytes))) => {
                    // We always respond to Ping messages, even when we're
                    // waiting on a specific message:
                    let payload_len = bytes.len();
                    self.send_ws_message(ws::Message::Pong(bytes))
                        .await
                        .with_context(|| {
                            format!(
                                "Replying to Ping WebSocket message (with \
                                 {payload_len} byte payload) from supervisor {}",
                                self.supervisor_id
                            )
                        })
                        .shutdown_on_err()?;
                }

                Some(Ok(ws::Message::Pong(_))) => {
                    let mut last_received_pong = self
                        .last_received_pong
                        .lock()
                        .expect("`last_received_pong` lock poisoned!");
                    *last_received_pong = tokio::time::Instant::now();
                }

                Some(Ok(ws::Message::Close(opt_close_frame))) => {
                    let close_frame_message = opt_close_frame
                        .as_ref()
                        .map(|cf| format!("({}) {}", cf.code, cf.reason))
                        .unwrap_or("<no close frame>".to_string());

                    return match (opt_close_frame.map(|cf| cf.code), allow_ws_close) {
                        // Normal websocket close, expected:
                        (Some(ws::close_code::NORMAL), true) => ControlFlowResult::ok(None),

                        // Normal websocket close, unexpected:
                        (Some(ws::close_code::NORMAL), false) => Err(anyhow!(
                            "WebSocket closed by supervisor {} while \
                             waiting for message: \"{close_frame_message}\"",
                            self.supervisor_id
                        ))
                        .shutdown_on_err(),

                        // Abnormal websocket close, unexpected:
                        (close_frame_code, _) => Err(anyhow!(
                            "Abnormal WebSocket close (code \
                             {close_frame_code:?}) by supervisor {} while \
                             waiting for message: \"{close_frame_message}\"",
                            self.supervisor_id
                        ))
                        .shutdown_on_err(),
                    };
                }

                Some(Ok(message)) => {
                    if let Some(ret) = filter(message.clone())? {
                        return ControlFlowResult::ok(Some(ret));
                    } else {
                        tracing::warn!("Discarding incoming message from supervisor: {message:?}");
                    }
                }

                None => {
                    return Err(anyhow!(
                        "Supervisor {} WebSocket stream is closed while \
                         waiting for message",
                        self.supervisor_id,
                    ))
                    .shutdown_on_err();
                }
            }
        }
    }

    async fn wait_for_message<R>(
        &self,
        allow_ws_close: bool,
        filter: impl Fn(api::switchboard_supervisor::Message) -> ControlFlowResult<Option<R>>,
    ) -> ControlFlowResult<Option<R>> {
        let supervisor_id = self.supervisor_id;
        self.wait_for_ws_message(allow_ws_close, |ws_message| match ws_message {
            ws::Message::Text(text_message) => {
                let message: api::switchboard_supervisor::Message = serde_json::from_str(
                    &text_message,
                )
                .with_context(|| {
                    format!(
                        "Deserializing incoming WebSocket message from supervisor {supervisor_id}",
                    )
                })
                .shutdown_on_err()?;

                filter(message)
            }

            // These message types are ignored:
            ws::Message::Binary(_) => ControlFlowResult::ok(None),

            // These message types are already handled in the
            // `wait_for_ws_message` function:
            ws::Message::Ping(_) | ws::Message::Pong(_) | ws::Message::Close(_) => {
                ControlFlowResult::ok(None)
            }
        })
        .await
    }

    async fn process_supervisor_status(
        &self,
        status_message: api::switchboard_supervisor::ReportedSupervisorStatus,
    ) -> ControlFlowResult<()> {
        tracing::debug!("Received supervisor current status: {status_message:?}");

        // We've received an update of the current supervisor status. At this
        // point we want to:
        //
        // - Check that the supervisor's status is in-sync with the database
        //   state (specifically, that it's executing the "right" job)
        //
        // - Update detailed status information of the job in the database (such
        //   as its sub-state (e.g., starting, running, stopping, ...)

        // We start by checking that the supervisor and database agree on which
        // job should be running, if any:
        //
        // Compare with the `current_job_id` in the database. We may also need
        // to inspect the job's `functional_state`, so we retrieve both in a
        // serializable transaction to have a consistent view of the database:
        let supervisor_id = self.supervisor_id;
        let (db_supervisor, db_current_job) = self
            .worker_instance_id_check_txn(async |txn: &mut Transaction<'_, Postgres>| {
                let db_supervisor = sql::supervisor::get(supervisor_id, &mut **txn)
                    .await
                    .with_context(|| format!("Retrieving supervisor {supervisor_id} DB row"))
                    .shutdown_on_err()?;

                let db_current_job_fut = db_supervisor.current_job.into_option().map(async |job_id| {
                    ControlFlowResult::ok(
                        sql::job::get(job_id, &mut **txn)
                            .await
                            .with_context(|| {
                                format!(
                                    "Retrieving job {job_id} DB row (current_job \
				     of supervisor {supervisor_id})"
                                )
                            })
                            .shutdown_on_err()?,
                    )
                });

                let db_current_job = match db_current_job_fut {
                    None => None,
                    Some(fut) => Some(fut.await?),
                };

                ControlFlowResult::ok((db_supervisor, db_current_job))
            })
            .await?;

        // Sanity check: we retrieved both the supervisor and its current job in
        // a serializable transaction. The supervisor's `current_job` must point
        // to a valid job record:
        assert_eq!(
            db_supervisor.current_job.into_option().is_some(),
            db_current_job.is_some(),
            "Supervisor {} indicates it has a `current_job` (job_id = {:?}), \
	     but no such record exists (db_current_job = {:?})!",
            self.supervisor_id,
            db_supervisor.current_job,
            db_current_job,
        );

        // Now, reconcile the supervisor's status and database state:
        match (db_current_job, status_message) {
            (None, api::switchboard_supervisor::ReportedSupervisorStatus::Idle) => {
                // Supervisor is idle, and has no job assigned in the
                // database. Nothing to do.
                tracing::debug!("Both supervisor and database indicate idle state, in sync!");
                ControlFlowResult::ok(())
            }

            (
                Some(sql::job::SqlJob {
                    job_id: db_job_id, ..
                }),
                api::switchboard_supervisor::ReportedSupervisorStatus::Idle,
            ) => {
                // Supervisor believes job should be running, but reported
                // supervisor status is Idle.
                tracing::info!(
                    "Supervisor indicates it is idle, but database indicates \
		     it should be executing job {db_job_id}, reconciling..."
                );

                // Check if the job was already dispatched: in that case, it
                // looks like the supervisor crashed before it could signal
                // termination of that job. We won't restart such jobs, as they
                // are not necessarily idempotent. We do run the handler for
                // determining what to do with these failed jobs. This handler
                // will unassign it from the supervisor, returning both to idle.
                //
                // If the job was not already dispatched, but is rather in the
                // `dispatching` state, that means that either we, the
                // supervisor, or the WebSocket connection crashed before the
                // supervisor has received the job start request. We resend it
                // in this case.
                todo!()
            }

            (
                None,
                api::switchboard_supervisor::ReportedSupervisorStatus::OngoingJob {
                    job_id: supervisor_job_id,
                    ..
                },
            ) => {
                // The supervisor is running a job, even though it shouldn't be!
                tracing::info!(
                    "Supervisor indicates it is running job \
		     {supervisor_job_id}, but database indicates it should be \
		     idle, reconciling..."
                );

                // This can be the result of a supervisor going offline for an
                // extended period of time, and the job being "pronounced dead"
                // by the switchboard or having hit its timeout. In this case,
                // we ask the supervisor to terminate it.
                // self.request_cancel_job().await
                todo!()
            }

            (
                Some(sql::job::SqlJob {
                    job_id: db_job_id, ..
                }),
                api::switchboard_supervisor::ReportedSupervisorStatus::OngoingJob {
                    job_id: supervisor_job_id,
                    ..
                },
            ) if db_job_id == supervisor_job_id.into() => {
                // The supervisor is running a job that's assigned to it in the
                // database. The database constraints guarantee that this job is
                // in an "executing" state (one that's supposed to be starting /
                // running / stopping) on a supervisor. Thus, we don't have to
                // do any addl. checks here -- the state is in sync!
                tracing::debug!(
                    "Both supervisor and database indicate job {db_job_id} is \
		     active, in sync!"
                );
                ControlFlowResult::ok(())
            }

            (
                Some(sql::job::SqlJob {
                    job_id: db_job_id, ..
                }),
                api::switchboard_supervisor::ReportedSupervisorStatus::OngoingJob {
                    job_id: supervisor_job_id,
                    ..
                },
            ) => {
                // The supervisor is running a job that's different from the one
                // we expect it to run. This case is weird: we'd always expect a
                // supervisor to transition back to `idle` before we attempt to
                // dispatch another job. If this happens, something's off --
                // perhaps there are two switchboard instances that the
                // supervisor has switched between?
                tracing::warn!(
                    "Supervisor indicates it is running job \
		     {supervisor_job_id}, but database indicates it should be \
		     running a *different* job {db_job_id}. This likely \
		     indicates some configuration issue or bug! Reconciling..."
                );

                // Either case, we can recover this state, by terminating the
                // currently running job and, depending on the state of the
                // database's Job state, dispatch it (for `dispatching`) or
                // pronounce it dead (for `dispatched`, as jobs may not be
                // idempotent).
                todo!()
            }
        }
    }

    // Force-resync the supervisor's status with the database state. This
    // function only returns after the supervisor has responded with its current
    // status, and the worker has initiated any actions required to reconcile
    // the states:
    async fn resync_status(&self) -> ControlFlowResult<()> {
        // Send a message querying the current supervisor status:
        let status_request_id = Uuid::new_v4();
        self.send_message(api::switchboard_supervisor::Message::StatusRequest(
            api::switchboard_supervisor::Request {
                request_id: status_request_id,
                message: (),
            },
        ))
        .await
        .with_context(|| {
            format!(
                "Sending initial status request to supervisor {}",
                self.supervisor_id
            )
        })
        .shutdown_on_err()?;

        // Up until we get the response to the above message, we ignore all other
        // incoming messages:
        let supervisor_status_resp = self
            .wait_for_message(
                // Closing the connection shall be interpreted as an error
                // condition, and is not expected:
                false,
                // Filter for (and map) just the current status response message:
                |message| match message {
                    api::switchboard_supervisor::Message::StatusResponse(resp)
                        if resp.response_to_request_id == status_request_id =>
                    {
                        ControlFlowResult::ok(Some(resp.message))
                    }
                    _ => ControlFlowResult::ok(None),
                },
            )
            .await?
            .expect(
                "SupervisorWSWorker::wait_for_message yielded None, despite \
                 connection shutdowns treated as errors!",
            );

        self.process_supervisor_status(supervisor_status_resp).await
    }

    // Dispatch the next enqueued job that is assigned to this supervisor, if
    // there is one and we're not currently executing a different job.
    async fn dispatch_next_enqueued(&self) -> ControlFlowResult<()> {
        let dispatch_job_id = self.worker_instance_id_check_txn(async |txn: &mut Transaction<'_, Postgres>| {
            let db_supervisor = sql::supervisor::get(self.supervisor_id, &mut **txn)
                .await
                .with_context(|| format!("Retrieving supervisor {} DB row", self.supervisor_id))
                .shutdown_on_err()?;

            // Check to ensure that there is not currently a job active on this
            // supervisor:
            if db_supervisor.current_job.into_option().is_some() {
                return ControlFlowResult::ok(None);
            }

            // There is no current job, retrieve the next to-be dispatched job
            // on this supervisor, if there is one:
            let opt_job_id = sql::job::find_next_enqueued_for_supervisor(self.supervisor_id, &mut **txn)
		.await
		.with_context(|| format!("Retreiving next enqueued job for supervisor {}", self.supervisor_id))
		.shutdown_on_err()?;

	    if let Some(job_id) = opt_job_id {
		// There is a job to dispatch, and the supervisor is currently
		// idle. We change the job's state to `dispatching` and mark it
		// as the supervisor's `current_job`:
		sql::job::update_job_state(
		    job_id,
		    sql::job::SqlJobState::Dispatching,
		    &mut **txn,
		)
		    .await
		    .with_context(|| format!("Setting state of job {} to 'dispatching' (to be dispatched on supervisor {})", job_id, self.supervisor_id))
		    .continue_on_err()?;

		sql::supervisor::set_current_job(
		    self.supervisor_id,
		    job_id,
		    &mut **txn,
		)
		    .await
		    .with_context(|| format!("Setting current_job = {} for supervisor {}", job_id, self.supervisor_id))
		    .continue_on_err()?;
	    }

	    ControlFlowResult::ok(opt_job_id)
        })
            .await?;

	todo!()
    }

    async fn loop_iter(&mut self) -> ControlFlowResult<()> {
        let mut ping_interval = self.ping_interval.try_lock().expect(
            "Failed to acquire immediate lock on `ping_timeout`, reentrant call to `loop_iter`?",
        );
        let mut check_enqueued_jobs_interval = self.check_enqueued_jobs_interval.try_lock()
	    .expect("Failed to acquire immediate lock on `check_enqueued_jobs_interval`, reentrant call to `loop_iter`?");

        #[rustfmt::skip]
        tokio::select! {
	    biased;

	    // Does not call `self.send_ws_message` during the Future's
	    // execution. This has highest priority: if we're processing a flood
	    // of messages from the supervisor, we want to generally try and
	    // keep the connection alive by sending Ping messages or, terminate
	    // if when we haven't received a Pong for the specified timeout
	    // (which would be symptomatic of a busy-loop send bug on the
	    // switchboard). Eventually, we'll want to employ some better
	    // rate-limiting.
            _ = ping_interval.tick() => {
                let elapsed = {
		    let last_received_pong = self.last_received_pong.lock().expect("`last_received_pong` lock poisoned");
		    last_received_pong.elapsed()
		};
                if elapsed > self.keepalive_timeout {
                    return ControlFlowResult::ShutdownError(anyhow!(
			"Haven't received a PONG from supervisor {} in {elapsed:?}",
			self.supervisor_id
		    ));
                }

		tracing::trace!("Supervisor WebSocket PING");
                if let Err(e) = self.send_ws_message(ws::Message::Ping(vec![])).await {
                    return ControlFlowResult::ShutdownError(anyhow!(
			"Failed to send PING to supervisor {}: {e}",
			self.supervisor_id,
		    ));
                }

		ControlFlowResult::ok(())
            }

	    // This is the only branch that may (transitively) call
	    // `self.send_ws_message` interally, during execution of its
	    // future. No other branch is permitted to do this, as otherwise we
	    // could attempt to lock the `socket_sink` Mutex twice and panic.
            res = self.wait_for_message(true, |_| ControlFlowResult::ok(None::<()>)) => {
                res?;
                ControlFlowResult::ok(())
            }

	    // Run a periodic check looking for enqueued jobs that have been
	    // assigned to this supervisor (and we should thus select as
	    // `current_job` and dispatch):
	    _ = check_enqueued_jobs_interval.tick() => {
		todo!()
	    }
        }
    }
}

// ===== ControlFlowResult helper infrastructure: =====

enum ControlFlowResult<T, E = anyhow::Error> {
    Continue(Result<T, E>),
    ShutdownSuccess,
    ShutdownError(E),
}

impl<T, E> ControlFlowResult<T, E> {
    pub fn ok(v: T) -> Self {
        ControlFlowResult::Continue(Ok(v))
    }

    pub fn into_result(self) -> Result<Option<T>, E> {
        match self {
            ControlFlowResult::Continue(res) => res.map(Some),
            ControlFlowResult::ShutdownSuccess => Ok(None),
            ControlFlowResult::ShutdownError(e) => Err(e),
        }
    }

    pub fn is_err(&self) -> bool {
        match self {
            ControlFlowResult::Continue(res) => res.is_err(),
            ControlFlowResult::ShutdownSuccess => false,
            ControlFlowResult::ShutdownError(_) => true,
        }
    }

    pub fn is_shutdown(&self) -> bool {
        match self {
            ControlFlowResult::Continue(_) => false,
            ControlFlowResult::ShutdownSuccess => true,
            ControlFlowResult::ShutdownError(_) => true,
        }
    }
}

trait IntoControlFlowResult<T, E> {
    fn continue_on_err(self) -> ControlFlowResult<T, E>;
    fn shutdown_on_err(self) -> ControlFlowResult<T, E>;
}

impl<T, E> IntoControlFlowResult<T, E> for Result<T, E> {
    fn continue_on_err(self) -> ControlFlowResult<T, E> {
        match self {
            Ok(v) => ControlFlowResult::Continue(Ok(v)),
            Err(e) => ControlFlowResult::Continue(Err(e)),
        }
    }

    fn shutdown_on_err(self) -> ControlFlowResult<T, E> {
        match self {
            Ok(v) => ControlFlowResult::Continue(Ok(v)),
            Err(e) => ControlFlowResult::ShutdownError(e),
        }
    }
}

impl<T, E> std::ops::Try for ControlFlowResult<T, E> {
    type Output = T;
    type Residual = ControlFlowResult<std::convert::Infallible, E>;

    fn from_output(output: Self::Output) -> Self {
        ControlFlowResult::Continue(Ok(output))
    }

    fn branch(self) -> std::ops::ControlFlow<Self::Residual, Self::Output> {
        match self {
            ControlFlowResult::Continue(Ok(v)) => std::ops::ControlFlow::Continue(v),
            ControlFlowResult::Continue(Err(e)) => {
                std::ops::ControlFlow::Break(ControlFlowResult::Continue(Err(e)))
            }
            ControlFlowResult::ShutdownSuccess => {
                std::ops::ControlFlow::Break(ControlFlowResult::ShutdownSuccess)
            }
            ControlFlowResult::ShutdownError(e) => {
                std::ops::ControlFlow::Break(ControlFlowResult::ShutdownError(e))
            }
        }
    }
}

impl<T, E> std::ops::FromResidual for ControlFlowResult<T, E> {
    fn from_residual(residual: <Self as std::ops::Try>::Residual) -> Self {
        match residual {
            ControlFlowResult::Continue(Ok(infallible)) => match infallible {},
            ControlFlowResult::Continue(Err(e)) => ControlFlowResult::Continue(Err(e)),
            ControlFlowResult::ShutdownSuccess => ControlFlowResult::ShutdownSuccess,
            ControlFlowResult::ShutdownError(e) => ControlFlowResult::ShutdownError(e),
        }
    }
}

// Code graveyard:

// pub struct SupervisorWSStreamTask {
//     socket: WebSocket,
//     shutdown_req: tokio::sync::watch::Receiver<bool>,
//     incoming_ws_message_sink: tokio::sync::mpsc::Sender<Result<ws::Message, axum::Error>>,
//     outgoing_ws_message_source: tokio::sync::mpsc::Receiver<ws::Message>,
//     pending_incoming_message: Option<ws::Message>,
// }

// impl SupervisorWSStreamTask {
//     pub async fn run(self) -> Result<(), Option<axum::Error>> {
//         loop {
//             #[rustfmt::skip]
//          tokio::select! {
//              // Follow the order of branches as specified here:
//              biased;

//              // Check for shutdown requests first:
//              res = self.shutdown_req.wait_for(|shutdown| *shutdown) => {
//                  return Ok(res.map(|_| ()).expect(
//                      "SupervisorWSStreamTask shutdown_req channel closed!"
//                  ));
//              },

//              // Prioritize sending over receiving messages. If not, we could
//              // be producing a deadlock: the stream task is waiting on space
//              // in the receive channel, while the WSWorker can't receive new
//              // messages as it is stuck trying to send messages through the
//              // send channel:
//              res = self.outgoing_ws_message_source.recv() => {
//                  let message = res.expect(
//                      "SupervisorWSStreamTask's outgoing_ws_message channel \
//                       has been closed!"
//                  );

//                  self.socket.send(message).await.map_err(Some)?;
//              }

//              // We forward received messages in a two-step process, instead
//              // of a single branch. This is to avoid receiving a message from
//              // the WebSocket channel, but then entirely blocking this task
//              // because the channel's full:
//              res = self.incoming_ws_message_sink.send(
//                  self.pending_incoming_message.clone().unwrap()
//              ), if self.pending_incoming_message.is_some() => {
//                  // The message was successfully sent, remove it from the
//                  // pending `Option`:
//                  self.pending_incoming_message.take();
//              }

//              opt = self.socket.recv(), if self.pending_incoming_message.is_none() => {
//                  // Return with an Err(None) if the WebSocket is closed.
//                  // Unfortunately, `.transpose` behaves slightly different.
//                  let message: ws::Message = match opt {
//                      None => Err(None),
//                      Some(Err(e)) => Err(Some(e)),
//                      Some(Ok(msg)) => Ok(msg),
//                  }?;

//                  self.pending_incoming_message.replace(message);
//              }
//          }
//         }
//     }
// }

// ws_stream_task_handle: tokio::task::JoinHandle<Result<(), Option<axum::Error>>>,
// ws_stream_task_shutdown_req: tokio::sync::watch::Sender<bool>,
// ws_stream_task_incoming_message_source:
//     tokio::sync::mpsc::Receiver<Result<ws::Message, axum::Error>>,
// ws_stream_task_outgoing_message_sink: tokio::sync::mpsc::Sender<ws::Message>,

// // Now, start a dedicated task to handle the WebSocket stream.
// //
// // This is required because this struct does not implement `Sync`, so
// // keeping it stored in our `SupervisorWSWorker` struct will make the
// // future returned by this function be `!Send`.
// //
// // Also, the `WebSocket` requires a mutable reference for `send` and
// // `recv`, so we'd need to put it behind a `Mutex` or require each
// // method of this struct to take a `&mut self` reference -- which
// // doesn't work with helpers of the form (`self.helper(closure)`), where
// // both the helper and the closure need `&mut self`. And the
// // `MutexGuard` would inherit the `WebSocket`'s `!Sync` bound, making it
// // unable to be kept across await points...
// let (ws_stream_task_shutdown_req, shutdown_req) = tokio::sync::watch::channel(false);
// let (incoming_ws_message_sink, ws_stream_task_incoming_message_source) =
//     tokio::sync::mpsc::channel(1);
// let (ws_stream_task_outgoing_message_sink, outgoing_ws_message_source) =
//     tokio::sync::mpsc::channel(1);
// let ws_stream_task = SupervisorWSStreamTask {
//     socket,
//     shutdown_req,
//     incoming_ws_message_sink,
//     outgoing_ws_message_source,
//     pending_incoming_message: None,
// };
// let ws_stream_task_handle = tokio::task::spawn(ws_stream_task.run());

// ws_stream_task_handle,
// ws_stream_task_shutdown_req,
// ws_stream_task_incoming_message_source,
// ws_stream_task_outgoing_message_sink,
