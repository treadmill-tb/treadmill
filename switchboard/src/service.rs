use crate::auth::AuthorizationSource;
use crate::auth::db::DbAuth;
use crate::config::ServiceConfig;
use crate::perms::RunJobOnSupervisor;
use crate::service::herd::{Herd, HerdError, ReservationError};
use crate::service::kanban::{Kanban, KanbanError};
use crate::service::tag::NaiveTagConfig;
use crate::sql::api_token::TokenError;
use crate::sql::job::history::SqlJobEvent;
use crate::sql::job::{SqlExitStatus, SqlFunctionalState, SqlJob, SqlRestartPolicy};
use crate::sql::{self, SqlSshEndpoint};
use axum::extract::ws::WebSocket;
use chrono::{DateTime, TimeDelta, Utc};
use futures_util::stream::FuturesOrdered;
use futures_util::{FutureExt, StreamExt};
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, PgPool, Postgres};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{Instrument, instrument};
use treadmill_rs::api::switchboard::{
    ExitStatus, ExtendedJobState, JobEvent, JobRequest, JobResult, JobSshEndpoint, JobState,
    JobStatus, SupervisorStatus,
};
use treadmill_rs::api::switchboard_supervisor;
use treadmill_rs::api::switchboard_supervisor::{
    ImageSpecification, ReportedSupervisorStatus, SupervisorJobEvent,
};
use treadmill_rs::connector::RunningJobState;
use uuid::Uuid;

pub mod herd;
pub mod kanban;
mod socket_connection;
mod tag;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("Database error: {0}")]
    Database(sqlx::Error),
    #[error("Herd error: {0}")]
    Herd(HerdError),
    #[error("Reservation error: {0}")]
    Reservation(ReservationError),
    #[error("Kanban error: {0}")]
    Kanban(KanbanError),
    #[error("Job's tag configuration did not match any currently registered supervisors")]
    SupervisorMatchError,
    #[error("Job deleted between match and launch")]
    NoLongerQueued,
    #[error("No such job")]
    NoSuchJob,
    #[error("Supervisor already connected")]
    SupervisorAlreadyConnected,
}

struct State {
    kanban: Kanban,
    herd: Herd,
}
pub struct Service {
    state: Mutex<State>,
    pool: PgPool,
    service_config: ServiceConfig,
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: SUPERVISOR INITIALISATION / NORMALISATION
// -------------------------------------------------------------------------------------------------

impl Service {
    // ---- PUBLIC ----

    #[instrument(skip(pool, service_config))]
    pub async fn new(
        pool: PgPool,
        service_config: ServiceConfig,
    ) -> Result<Arc<Self>, ServiceError> {
        let this = Arc::new(Self {
            state: Mutex::new(State {
                kanban: Kanban::new(),
                herd: Herd::new(),
            }),
            pool,
            service_config,
        });

        // TODO: make sure there are no cases of multiple jobs running on the same supervisor

        // Load supervisor table from the database.
        this.load_supervisors().await?;

        // Now, recover the state from before the switchboard last exited

        // First, load the set of 'running' jobs and separate it into jobs that have hit timeout and
        // jobs that have not. Anything that has hit timeout, we will then try to restart.
        // The simplest way to 'restart', in this case, is to insert a new rows in the database,
        // which `normalize_queued_job_states()` and `load_job_queue_from_vec()` will then pick up
        // and rebuild, so we handle active jobs before queued jobs.
        let (active_jobs, maybe_restartable) = this.normalize_active_job_states().await?;
        this.load_active_jobs_from_vec(active_jobs).await;
        this.try_restart_expired_jobs(maybe_restartable).await?;

        // Load the set of 'queued' jobs, prune out any that timed out in the queue, and load the
        // rest in the kanban.
        let queued_jobs = this.normalize_queued_job_states().await?;
        this.load_job_queue_from_vec(queued_jobs).await?;

        // Launch the match daemon
        Arc::clone(&this).launch_match_daemon();

        Ok(this)
    }

    // ---- INTERNALS ----

    fn launch_match_daemon(self: Arc<Self>) {
        // Note that this is not efficient: runs O(nm) in n jobs, m connected supervisors.
        // This can be improved to a linear factor of O(n) every time a supervisor becomes idle, and
        // O(m) every time a job is added.
        // However, this is considerably more complicated, especially if you don't want to hold down
        // the state lock the whole time, especially with the way `launch` is currently implemented.
        // Eventually, it'd be nice to transition to that architecture, but for now, I'm hacking my
        // way around that.

        tokio::spawn({
            let span = tracing::info_span!("(match daemon)");
            async move {
                tracing::info!("Starting match daemon...");
                let duration = self
                    .service_config
                    .match_interval
                    .to_std()
                    .expect("configured match interval is out of range");
                loop {
                    tokio::time::sleep(duration).await;

                    let mut matches = vec![];

                    {
                        let state = self.state.lock().await;
                        let mut idle_set = state.herd.currently_idle();

			// Randomize the set of idle supervisors to better
			// balance the load and wear of the installed boards:
			rand::seq::SliceRandom::shuffle(&mut idle_set[..], &mut rand::thread_rng());

                        let mut queued_jobs = state.kanban.get_queued_jobs();
                        for supervisor_id in idle_set {
                            let mut matched_to = None;
                            for (idx, (job_id, job_matches)) in queued_jobs.iter().enumerate() {
                                if job_matches.contains(&supervisor_id) {
                                    matches.push((supervisor_id, *job_id));
                                    matched_to = Some(idx);
                                    break;
                                }
                            }
                            if let Some(idx) = matched_to {
                                queued_jobs.swap_remove(idx);
                            }
                        }
                        // drop the mutex BEFORE we try to start launching stuff
                        drop(state);
                    }

                    // TODO: SCHEDULED state as intermediary?

                    for (supervisor_id, job_id) in matches {
                        tracing::debug!("Launching job ({job_id}) on supervisor ({supervisor_id})");
                        if let Err(e) = self.launch(job_id, supervisor_id, Utc::now()).await {
                            tracing::error!("Failed to launch job ({job_id}) on supervisor ({supervisor_id}): {e}");
                        }
                    }
                }
            }
                .instrument(span)
        });
    }

    async fn load_supervisors(&self) -> Result<(), ServiceError> {
        tracing::info!("Loading supervisor table...");

        let mut state = self.state.lock().await;
        let supervisors = sql::supervisor::fetch_all_supervisors(&self.pool)
            .await
            .map_err(ServiceError::Database)?;
        let supervisor_count = supervisors.len();
        for supervisor in supervisors {
            state
                .herd
                .register_supervisor(
                    supervisor.supervisor_id,
                    BTreeSet::from_iter(supervisor.tags.into_iter()),
                )
                .map_err(ServiceError::Herd)?;
        }

        tracing::info!("herd: {:?}", state.herd);
        drop(state);

        tracing::info!("Successfully loaded supervisor table ({supervisor_count} supervisors).");

        Ok(())
    }

    async fn normalize_active_job_states(
        &self,
    ) -> Result<(Vec<SqlJob>, Vec<SqlJob>), ServiceError> {
        let now = Utc::now();

        tracing::info!("loading / normalizing active jobs...");

        let mut current_active = vec![];
        let mut maybe_restartable = vec![];

        let active_jobs = sql::job::fetch_all_dispatched(&self.pool)
            .await
            .map_err(ServiceError::Database)?;
        let active_count = active_jobs.len();
        for job in active_jobs {
            let timeout = job.timeout();
            // PANIC SAFETY: for this to be None would violate one of the CHECK constraints.
            let started_at = *job.started_at().unwrap();
            if started_at > now || (now - started_at) >= timeout {
                // CANCEL & RESTART THE JOB - cancellation is thankfully just the database end, and
                // the restart mechanism hasn't been implemented yet.
                let mut tx = self.pool.begin().await.map_err(ServiceError::Database)?;
                let terminated_at = Utc::now();
                let exit_status = ExitStatus::JobTimeout;
                let job_result = JobResult {
                    job_id: job.job_id(),
                    supervisor_id: None,
                    exit_status,
                    host_output: None,
                    terminated_at,
                };
                sql_update_jobs_table_for_termination(&job_result, &mut tx)
                    .await
                    .map_err(ServiceError::Database)?;
                sql::job::history::insert(
                    job.job_id(),
                    JobEvent::SetExitStatus {
                        exit_status,
                        status_message: None,
                    },
                    terminated_at,
                    tx.as_mut(),
                )
                .await
                .map_err(ServiceError::Database)?;
                sql::job::history::insert(
                    job.job_id(),
                    JobEvent::FinalizeResult { job_result },
                    terminated_at,
                    tx.as_mut(),
                )
                .await
                .map_err(ServiceError::Database)?;
                tx.commit().await.map_err(ServiceError::Database)?;

                maybe_restartable.push(job);
            } else {
                current_active.push(job);
            }
        }

        tracing::info!(
            "Successfully loaded / normalized active jobs; {}/{} jobs expired and were cleaned up.",
            maybe_restartable.len(),
            active_count,
        );

        Ok((current_active, maybe_restartable))
    }

    async fn load_active_jobs_from_vec(self: &Arc<Self>, jobs: Vec<SqlJob>) {
        tracing::info!("Loading active jobs...");
        let count = jobs.len();

        let mut state = self.state.lock().await;

        for job in jobs {
            tracing::debug!("Adding job ({}) as active", job.job_id());
            let job_times_out_at = *job
                .started_at()
                .expect("job marked as 'running' in the database must have `started_at` non-null")
                + job.timeout();
            let job_times_out_at_instant = datetime_to_instant_approx(job_times_out_at).expect(
                "job's started_at and timeout as specified in the database caused on overflow",
            );
            let supervisor_id = job.dispatched_on_supervisor_id().expect(
                "job marked as 'running' in the database must have `running_on_supervisor_id` non-null"
            );

            // normal launch process:
            //  reserve
            //  send start message
            //  set state to 'running' in database
            //  activate in kanban (analogue)
            let (stop_tx, stop_rx) = oneshot::channel();
            let (jsr, close_tx) = state
                .kanban
                .add_active_job(job.job_id(), supervisor_id, None, stop_tx)
                .expect("Failed to add active job");

            // let jsr = launch_job_status_tee(self.pool.clone(), job.job_id(), jsr);

            // XXX(max): I think this isn't actually necessary here; it won't produce a deadlock in
            // launch_termination_watchdog, just force it to wait for a bit extra.
            // drop(state);

            let (fwd_confluence,) = Arc::clone(self).launch_job_watchdog(
                job.job_id(),
                supervisor_id,
                job_times_out_at_instant,
                jsr,
                close_tx,
                stop_rx,
            );
            Arc::clone(self).launch_event_recorder(job.job_id(), supervisor_id, fwd_confluence);
        }

        drop(state);

        tracing::info!("Successfully loaded {count} active jobs.")
    }

    async fn try_restart_expired_jobs(
        self: &Arc<Self>,
        jobs: Vec<SqlJob>,
    ) -> Result<(), ServiceError> {
        tracing::info!("Attempting to restart expired jobs...");
        let total_count = jobs.len();

        let restartable_jobs: Vec<SqlJob> = jobs
            .into_iter()
            .filter(|job| {
                job.restart_policy().remaining_restart_count > 0
                    && matches!(job.read_image_spec(), ImageSpecification::Image { .. })
            })
            .collect();
        let restartable_count = restartable_jobs.len();
        let mut restarted_count = 0;

        let state = self.state.lock().await;
        for original in restartable_jobs {
            // so, basically, we add another job, mostly as normal
            let queued_at = Utc::now();
            let new_job_id = Uuid::new_v4();

            sql_build_job_restart(new_job_id, original, queued_at, &self.pool)
                .await
                .map_err(ServiceError::Database)?;

            restarted_count += 1;
        }
        drop(state);

        tracing::info!(
            "Restarted {restarted_count}/{restartable_count} (eligible)/{total_count} (expired) jobs."
        );

        Ok(())
    }

    async fn normalize_queued_job_states(&self) -> Result<Vec<SqlJob>, ServiceError> {
        tracing::info!("loading / normalizing queued jobs...");

        let now = Utc::now();
        let mut current_queued = vec![];

        let queued_jobs = sql::job::fetch_all_queued(&self.pool)
            .await
            .map_err(ServiceError::Database)?;
        let queued_count = queued_jobs.len();
        for job in queued_jobs {
            let queued_at = *job.queued_at();
            if queued_at > now || (now - queued_at) >= self.service_config.default_queue_timeout {
                // CANCEL THE JOB - only need to do the database end of this, thankfully.
                sql_enforce_queue_timeout(job.job_id(), &self.pool)
                    .await
                    .map_err(ServiceError::Database)?;
            } else {
                current_queued.push(job);
            }
        }

        tracing::info!(
            "Successfully loaded / normalized queued jobs; {}/{} jobs expired and were cleaned up",
            queued_count - current_queued.len(),
            queued_count,
        );

        Ok(current_queued)
    }

    fn launch_queue_timeout_watchdog(
        self: &Arc<Self>,
        job_id: Uuid,
        expires_at: Instant,
        queue_removed: oneshot::Receiver<()>,
    ) {
        tracing::debug!(
            "Launching queue timeout watchdog for job {job_id}; expires at {expires_at:?}."
        );

        let this = Arc::clone(self);
        tokio::spawn({
            let span = tracing::info_span!("(job queue timeout watchdog)", %job_id);
            async move {
                let timeout_future = tokio::time::timeout_at(expires_at, queue_removed);
                match timeout_future.await {
                    Ok(Ok(())) => {
                        tracing::info!("Job ({job_id}) was removed from queue (presumed activated), canceling queue timeout watchdog.");
                        return;
                    }
                    Ok(Err(_)) => {
                        // TODO: what is the appropriate response?
                        panic!("Job ({job_id}) exhibiting abnormal behaviour: backing `QueuedJob` presumed to have dropped without sending queue exit notification, canceling queue timeout watchdog.");
                    }
                    // the `Elapsed` type is actually a unit struct, and thus carries no useful
                    // information, so we ignore the particular value
                    Err(_elapsed) => {
                        tracing::warn!("Job ({job_id}) timed out of queue")
                    }
                }

                // Job timed out, so we want to:
                //  - remove job from kanban queue
                //  - update info in the database
                let mut state = this.state.lock().await;
                if let Err(e) = state.kanban.remove_queued_job(job_id) {
                    tracing::error!("Failed to remove job ({job_id}) from queue: failed to remove job from kanban: {e}");
                }
                if let Err(e) = sql_enforce_queue_timeout(job_id, &this.pool).await {
                    tracing::error!("Failed to remove job ({job_id}) from queue: failed to update information in database: {e}");
                }
            }
                .instrument(span)
        });
    }

    async fn load_job_queue_from_vec(
        self: &Arc<Self>,
        jobs: Vec<SqlJob>,
    ) -> Result<(), ServiceError> {
        tracing::info!("Loading job queue...");
        let count = jobs.len();

        let mut state = self.state.lock().await;
        for job in jobs {
            let auth_source =
                match DbAuth::from_pool_with_token_id(&self.pool, job.enqueued_by_token_id()).await
                {
                    Ok(db_auth) => db_auth,
                    Err(e) => match e {
                        TokenError::InvalidToken => {
                            // so, token was deleted (not just expired, but actually DELETE'd from the
                            // database), which means we can't restart it.
                            // Therefore, we cancel it.
                            sql_stop_orphaned_job(job.job_id(), &self.pool)
                                .await
                                .map_err(ServiceError::Database)?;
                            continue;
                        }
                        TokenError::Database(e) => {
                            return Err(ServiceError::Database(e));
                        }
                    },
                };
            let allowed_supervisor_fut: FuturesOrdered<_> = state
                .herd
                .tag_sets()
                .iter()
                .map(|(&supervisor_id, _)| {
                    auth_source.authorize(RunJobOnSupervisor { supervisor_id })
                })
                .collect();
            let allowed_supervisor_ids: HashSet<Uuid> = allowed_supervisor_fut
                .filter_map(|x| async move { x.map(|y| y.action().supervisor_id).ok() })
                .collect()
                .await;
            let matching_supervisor_ids = tag::find_matching_supervisors::<NaiveTagConfig>(
                job.raw_tag_config(),
                state.herd.tag_sets(),
                allowed_supervisor_ids,
            );
            let queue_removed = state
                .kanban
                .add_queued_job(
                    job.job_id(),
                    matching_supervisor_ids.clone(),
                    *job.queued_at(),
                    self.service_config.default_queue_timeout,
                )
                .map_err(ServiceError::Kanban)?;

            let expires_at = datetime_to_instant_approx(*job.queued_at() + job.timeout()).expect(
                "job's queued_at and timeout as specified in the database caused an overflow",
            );

            self.launch_queue_timeout_watchdog(job.job_id(), expires_at, queue_removed);
        }

        drop(state);

        tracing::info!("Successfully loaded {count} jobs in job queue.");

        Ok(())
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: SUPERVISOR CONNECTION
// -------------------------------------------------------------------------------------------------

impl Service {
    #[instrument(skip(self, socket))]
    pub async fn supervisor_connected(
        self: &Arc<Self>,
        supervisor_id: Uuid,
        mut socket: WebSocket,
    ) -> Result<(), ServiceError> {
        let mut state = self.state.lock().await;

        // The route-handling logic will check that `supervisor_id` exists as part of the
        // authentication process; however, since the process goes
        //
        //  (initial request) -> authenticate -> (upgrade + WS handshake) -> supervisor_connected
        //
        // it's possible for the supervisor to be unregistered between authentication and this call
        //
        // TODO: can we prevent the supervisor from unregistering between authentication and
        //       upgrade?
        if let Err(e) = state.herd.assert_registered(supervisor_id) {
            if let Err(e) = socket
                .send(axum::extract::ws::Message::Close(Some(
                    axum::extract::ws::CloseFrame {
                        // TODO: is this the correct code
                        code: axum::extract::ws::close_code::NORMAL,
                        reason: "supervisor unregistered during WebSocket handshake".into(),
                    },
                )))
                .await
            {
                tracing::error!("Failed to send close frame: {e}, dropping connection");
            }
            let _ = socket;
            return Err(ServiceError::Herd(e));
        }

        if state.herd.is_supervisor_connected(supervisor_id) {
            return Err(ServiceError::SupervisorAlreadyConnected);
        }

        // Underlying job status extractor exits when the connected_supervisor has dropped and all
        // job status receivers have dropped.
        let (supervisor_termination_handle, connected_supervisor) =
            herd::prepare_supervisor_connection(
                supervisor_id,
                socket,
                // TODO: make this an `Arc`
                self.service_config.socket.clone(),
            );

        let connected_supervisor = Arc::new(Mutex::new(connected_supervisor));

        // let jobs = sql_fetch_running_jobs_by_supervisor_id(supervisor.supervisor_id, &self.pool)
        //     .await
        //     .map_err(ServiceError::Database)?;
        // if jobs.len() > 1 {
        //     // F*CK
        //     todo!("DATABASE IN DISORDER: CANCEL (ALL - 1) JOBS RUNNING ON SUPERVISOR");
        // }
        // let maybe_job = jobs.first().copied();

        let maybe_job_id = state.kanban.get_active_job_on(supervisor_id);
        if let Some(job_id) = maybe_job_id {
            tracing::debug!("Detected active job on supervisor ({supervisor_id}): job ({job_id})");
            let lg = connected_supervisor.lock().await;
            let ss = match lg.request_supervisor_status().await {
                Ok(ss) => ss,
                Err(e) => {
                    // Not connected
                    return Err(ServiceError::Herd(e));
                }
            };
            drop(lg);
            match ss {
                ReportedSupervisorStatus::OngoingJob {
                    job_id: running_job_id,
                    job_state: _,
                } => {
                    if job_id != running_job_id {
                        tracing::error!(
                            "Job mismatch: expected job ({job_id}), found running job ({running_job_id})."
                        );
                        // so, the current situation:
                        //  - according to the kanban/database: `job_id` is running on the
                        //    supervisor
                        //  - according to the supervisor: `running_job_id` is running.
                        // resolution: kill `job_id`
                        // If the following condition holds:
                        //  - according to the kanban: `running_job_id` is also running?
                        // then kill `running_job_id` as well.
                        if let Some(running_job) = state.kanban.get_active_job(running_job_id) {
                            if let Some(tx) = running_job.stop_tx.take() {
                                let _ = tx.send(ExitStatus::JobCanceled);
                            }
                        }
                        let active_job = state.kanban.get_active_job(job_id).unwrap();
                        if let Some(tx) = active_job.stop_tx.take() {
                            let _ = tx.send(ExitStatus::JobCanceled);
                        }
                        // The issue here is that neither of running_job and active_job has a
                        // current reservation--I hope--so we have to send that manually.
                        // The further complication is that we have to wait until the supervisor
                        // sends a Terminated message, and we don't want to hold onto the state lock
                        // for that long.
                        // Temporary solution: just return, and let this spin until it resolves
                        // TODO
                        tracing::info!("Normalizing job mismatch: sending stop message");
                        let lg = connected_supervisor.lock().await;
                        if let Err(e) = lg.stop_job(switchboard_supervisor::StopJobMessage {
                            job_id: running_job_id,
                        }) {
                            tracing::error!(
                                "Failed to normalize job mismatch: failed to send stop message: {e:?}"
                            );
                            // TODO: check how this maps with the return semantics of send_stop_message
                            return Err(ServiceError::Herd(e));
                        }
                        return Err(ServiceError::Herd(HerdError::InvalidCondition));
                        // drop(lg);
                        // // wait until Terminated?
                        // state
                        //     .herd
                        //     .supervisor_connected(supervisor_id, connected_supervisor)
                        //     .map_err(ServiceError::Herd)?;
                    } else {
                        tracing::info!(
                            "Detection successful: job ({job_id}) is running on supervisor ({supervisor_id})."
                        );
                        let mut reservation = state
                            .herd
                            .reserved_supervisor_connected(supervisor_id, connected_supervisor)
                            .await
                            .map_err(ServiceError::Herd)?;
                        let maybe_active_job = state.kanban.get_active_job(job_id).unwrap();
                        if let Err(_) = maybe_active_job
                            .job_status_receiver_sender
                            .send(reservation.take_job_status_receiver().unwrap())
                        {
                            // error if and only if JSR disconnected, which more or less means that the
                            // atomicity property got broken somewhere.
                            panic!(
                                "System in disorder: JSR disconnected on active job with state locked"
                            );
                        }
                        // TODO: resolve A-B-A reservation problem when disconnect gets processed after connect
                        maybe_active_job.current_reservation = Some(reservation);
                    }
                }
                ReportedSupervisorStatus::Idle => {
                    // Oops
                    tracing::warn!(
                        "Detected job drop: supervisor ({supervisor_id}) is no longer reporting job ({job_id}) as active."
                    );
                    let active_job = state.kanban.get_active_job(job_id).unwrap();
                    if let Some(tx) = active_job.stop_tx.take() {
                        let _ = tx.send(ExitStatus::SupervisorDroppedJob);
                    }
                    state
                        .herd
                        .supervisor_connected(supervisor_id, connected_supervisor)
                        .map_err(ServiceError::Herd)?;
                }
            }
        } else {
            // supervisor is idle
            tracing::info!("Detected idle supervisor");
            state
                .herd
                .supervisor_connected(supervisor_id, connected_supervisor)
                .map_err(ServiceError::Herd)?;
        }

        drop(state);

        self.launch_disconnection_watchdog(supervisor_id, supervisor_termination_handle);

        // TODO: anything else in here?

        Ok(())
    }

    fn launch_disconnection_watchdog(
        self: &Arc<Self>,
        supervisor_id: Uuid,
        supervisor_termination_handle: JoinHandle<()>,
    ) {
        let this = Arc::clone(self);
        tokio::spawn({
            let span = tracing::info_span!("(supervisor disconnection watchdog)", %supervisor_id);
            async move {
                if let Err(e) = supervisor_termination_handle.await {
                    tracing::error!("supervisor ({supervisor_id}) run loop panicked: {e}");
                } else {
                    tracing::info!("supervisor ({supervisor_id}) run loop exited successfully");
                }

                let mut state = this.state.lock().await;

                // if there's an active reservation, need to close that up
                let maybe_job_id = state.kanban.get_active_job_on(supervisor_id);
                if let Some(job_id) = maybe_job_id {
                    let active_job = state.kanban.get_active_job(job_id).unwrap();
                    active_job.current_reservation = None;

                    // TODO: is there anything else that we need to clean up?
                }

                // TODO: I think it's safe to ignore the potential error returns
                let _ = state.herd.supervisor_disconnected(supervisor_id);

                // TODO: is there anything else that we need to clean up for this supervisor?
            }
            .instrument(span)
        });
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: LAUNCHING & STOPPING JOBS
// -------------------------------------------------------------------------------------------------

impl Service {
    #[instrument(skip(self))]
    pub async fn register_job(
        self: &Arc<Self>,
        job_request: JobRequest,
        as_token_id: Uuid,
    ) -> Result<Uuid, ServiceError> {
        let job_id = Uuid::new_v4();

        // TODO: proper checking of job_timeout
        let job_timeout = job_request
            .override_timeout
            .and_then(|td| PgInterval::try_from(td).ok())
            .unwrap_or_else(|| {
                PgInterval::try_from(self.service_config.default_job_timeout)
                    .expect("default job timeout is invalid")
            });
        let job_timeout_td = TimeDelta::microseconds(job_timeout.microseconds)
            + TimeDelta::days(i64::from(job_timeout.days));

        let queued_at = Utc::now();

        let mut state = self.state.lock().await;
        // this isn't a great solution, but not really sure how else to do this.
        // I feel like it'd be better to just pass in an auth_source, but that runs into issues with
        // perms::submit_job, because submit_job wants a Privilege<'_, SubmitJob>, and the '_ is
        // tied to the DbAuth already.
        // Unfortunately, we also don't really want to make DbAuth Clone.
        // Thus, I'm a bit stumped.
        let auth_source = match DbAuth::from_pool_with_token_id(&self.pool, as_token_id).await {
            Ok(db_auth) => db_auth,
            Err(e) => match e {
                TokenError::InvalidToken => {
                    // so, token was deleted (not just expired, but actually DELETE'd from the
                    // database).
                    // TODO: better error?
                    tracing::warn!("Failed to build auth source: token was DELETE'd.");
                    return Err(ServiceError::SupervisorMatchError);
                }
                TokenError::Database(e) => {
                    return Err(ServiceError::Database(e));
                }
            },
        };
        let allowed_supervisor_fut: FuturesOrdered<_> = state
            .herd
            .tag_sets()
            .iter()
            .map(|(&supervisor_id, _)| auth_source.authorize(RunJobOnSupervisor { supervisor_id }))
            .collect();
        let allowed_supervisor_ids: HashSet<Uuid> = allowed_supervisor_fut
            .filter_map(|x| async move { x.map(|y| y.action().supervisor_id).ok() })
            .collect()
            .await;
        let matching_supervisor_ids = tag::find_matching_supervisors::<NaiveTagConfig>(
            &job_request.tag_config,
            state.herd.tag_sets(),
            allowed_supervisor_ids,
        );
        sql_register_job(
            job_request,
            job_id,
            as_token_id,
            job_timeout,
            queued_at,
            &self.pool,
        )
        .await
        .map_err(ServiceError::Database)?;
        if matching_supervisor_ids.is_empty() {
            tracing::warn!("Job matched to no supervisors");
            let mut tx = self.pool.begin().await.map_err(ServiceError::Database)?;
            let terminated_at = Utc::now();
            let job_result = JobResult {
                job_id,
                supervisor_id: None,
                exit_status: ExitStatus::SupervisorMatchError,
                host_output: None,
                terminated_at,
            };
            sql_update_jobs_table_for_termination(&job_result, &mut tx)
                .await
                .map_err(ServiceError::Database)?;
            sql::job::history::insert(
                job_id,
                JobEvent::SetExitStatus {
                    exit_status: ExitStatus::SupervisorMatchError,
                    status_message: None,
                },
                terminated_at,
                tx.as_mut(),
            )
            .await
            .map_err(ServiceError::Database)?;
            sql::job::history::insert(
                job_id,
                JobEvent::FinalizeResult { job_result },
                terminated_at,
                tx.as_mut(),
            )
            .await
            .map_err(ServiceError::Database)?;
            tx.commit().await.map_err(ServiceError::Database)?;

            return Err(ServiceError::SupervisorMatchError);
        }

        {
            let queue_removed = state
                .kanban
                .add_queued_job(
                    job_id,
                    matching_supervisor_ids,
                    queued_at,
                    self.service_config.default_queue_timeout,
                )
                .map_err(ServiceError::Kanban)?;
            let expires_at = datetime_to_instant_approx(queued_at + job_timeout_td)
                .expect("job's queued_at and timeout as specified in request caused an overflow");
            self.launch_queue_timeout_watchdog(job_id, expires_at, queue_removed);
        }

        drop(state);

        Ok(job_id)
    }

    #[instrument(skip(self))]
    pub async fn cancel_job_external(&self, job_id: Uuid) -> Result<(), ServiceError> {
        // okay, need to terminate whatever is going on
        let mut state = self.state.lock().await;
        let sql_job = sql::job::fetch_by_job_id(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;
        match sql_job.functional_state() {
            SqlFunctionalState::Finalized => {
                // Simple case: job is already inactive
                return Ok(());
            }
            SqlFunctionalState::Queued => {
                // Slightly more complicated: job is in the queue
                let queued_job = state
                    .kanban
                    .remove_queued_job(job_id)
                    .map_err(ServiceError::Kanban)?;
                // Disarm the queue timeout watchdog
                let _ = queued_job.exit_notifier.send(());
                let mut tx = self.pool.begin().await.map_err(ServiceError::Database)?;
                let terminated_at = Utc::now();
                let exit_status = ExitStatus::JobCanceled;
                let job_result = JobResult {
                    job_id,
                    supervisor_id: None,
                    exit_status,
                    host_output: None,
                    terminated_at,
                };
                sql_update_jobs_table_for_termination(&job_result, &mut tx)
                    .await
                    .map_err(ServiceError::Database)?;
                sql::job::history::insert(
                    job_id,
                    JobEvent::SetExitStatus {
                        exit_status,
                        status_message: None,
                    },
                    terminated_at,
                    tx.as_mut(),
                )
                .await
                .map_err(ServiceError::Database)?;
                sql::job::history::insert(
                    job_id,
                    JobEvent::FinalizeResult { job_result },
                    terminated_at,
                    tx.as_mut(),
                )
                .await
                .map_err(ServiceError::Database)?;
                tx.commit().await.map_err(ServiceError::Database)?;
            }
            SqlFunctionalState::Dispatched => {
                // Most complicated (although fairly simple on this end): job is currently running

                let active_job = state
                    .kanban
                    .get_active_job(job_id)
                    .ok_or(ServiceError::NoSuchJob)?;
                // Notify the job termination watchdog that job is being canceled. The watchdog will
                // do the rest.
                if let Some(tx) = active_job.stop_tx.take() {
                    let _ = tx.send(ExitStatus::JobCanceled);
                }
            }
        }

        Ok(())
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: LAUNCHING & STOPPING JOBS
// -------------------------------------------------------------------------------------------------

impl Service {
    #[instrument(skip(self))]
    async fn launch(
        self: &Arc<Self>,
        job_id: Uuid,
        supervisor_id: Uuid,
        started_at: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        // First, we need to fetch the information necessary to start the job.
        let started_instant = Instant::now();

        tracing::debug!("Fetching job info from database...");

        let sql_job = sql::job::fetch_by_job_id(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;
        let sql_params = sql::job::parameters::fetch_by_job_id(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;

        let job_timeout = sql_job.timeout();
        let job_times_out_instant = started_instant
            + job_timeout
                .to_std()
                .expect("job timeout is out of range for conversion to std `Duration`");

        let start_job_message = switchboard_supervisor::StartJobMessage {
            job_id,
            image_spec: sql_job.read_image_spec(),
            ssh_keys: sql_job.ssh_keys().to_vec(),
            restart_policy: sql_job.restart_policy(),
            parameters: sql_params,
        };

        let mut state = self.state.lock().await;

        tracing::debug!("Running job setup");

        if !state.kanban.queued_job_exists(job_id) {
            return Err(ServiceError::NoLongerQueued);
        }

        tracing::debug!("Setting job state to: Scheduled");

        if let Err(e) =
            sql_set_job_state_to_scheduled(started_at, job_id, supervisor_id, &self.pool)
                .await
                .map_err(ServiceError::Database)
        {
            tracing::error!("Failed to update job state in database: {e}, aborting launch");
            return Err(e);
        }

        tracing::debug!("Successfully update database");

        tracing::debug!("Reserving supervisor");

        let reservation = state
            .herd
            .reserve_supervisor(supervisor_id)
            .await
            .map_err(ServiceError::Herd)?;

        tracing::debug!("Successfully reserved supervisor ({supervisor_id})");

        if let Err(e) = reservation.start_job(start_job_message) {
            tracing::error!("Failed to send job start message: {e}, retracting reservation");
            // Since we failed to start the job, don't try to mark the job as started.

            // Note that any error conditions returned from unreserve supervisor (not connected, not
            // registered, not reserved) all indicate that a reservation is no longer held.
            drop(reservation);
            if let Err(e) = state.herd.unreserve_supervisor(supervisor_id) {
                tracing::warn!("Failed to unreserve supervisor ({supervisor_id}): {e}");
            }

            // If start_job() failed, then the supervisor connection was lost. We therefore may as
            // well update the Herd state. Note that this would happen anyway with the watchdog, but
            // this prevents the mutex from getting jammed.
            state
                .herd
                .supervisor_disconnected(supervisor_id)
                .map_err(ServiceError::Herd)?;

            tracing::debug!("Successfully retracted launch attempt");

            return Err(ServiceError::Reservation(e));
        }

        tracing::debug!("Successfully sent job start message to supervisor");

        // if let Err(e) = sql_set_job_state_to_running(started_at, job_id, supervisor_id, &self.pool)
        //     .await
        //     .map_err(ServiceError::Database)
        // {
        //     tracing::error!("Failed to update job state in database: {e}, retracting, stopping job and reservation.");
        //
        //     if let Err(e) = reservation.stop_job(switchboard_supervisor::StopJobMessage { job_id })
        //     {
        //         // not connected, which is... fine?
        //         tracing::warn!("Unable to send job stop message: {e}");
        //     } else {
        //         tracing::debug!("Successfully sent job stop message");
        //     }
        //
        //     drop(reservation);
        //     // possible errors: not connected, not reserved, not registered.
        //     // in all these cases, the supervisor is no longer considered as "reserved", therefore
        //     // no error shall be returned.
        //     if let Err(e) = state.herd.unreserve_supervisor(supervisor_id) {
        //         tracing::warn!("Failed to unreserve supervisor ({supervisor_id}): {e}");
        //     }
        //
        //     tracing::debug!("Successfully retracted launch attempt");
        //
        //     return Err(e);
        // }

        // tracing::debug!("Successfully updated job state in database");

        let (stop_tx, stop_rx) = oneshot::channel::<ExitStatus>();

        let (jsr, close_tx) = state
            .kanban
            .activate(job_id, supervisor_id, reservation, stop_tx)
            .expect("activation failure, despite job existing and being queued");

        tracing::debug!("Successfully activated job ({job_id})");

        tracing::debug!("Launching job status tee for job ({job_id})");

        // Upload job status events to the database, then forward them to `jsr`.
        // let jsr = launch_job_status_tee(self.pool.clone(), job_id, jsr);

        // Unlock the state mutex as early as possible.
        drop(state);

        // Launch termination watchdog
        let (fwd_confluence,) = Arc::clone(self).launch_job_watchdog(
            job_id,
            supervisor_id,
            job_times_out_instant,
            jsr,
            close_tx,
            stop_rx,
        );
        Arc::clone(self).launch_event_recorder(job_id, supervisor_id, fwd_confluence);

        tracing::debug!("Launching termination watchdog for job ({job_id})");

        Ok(())
    }

    #[instrument(skip(self, event_channel, confluence_close_tx, stop_rx))]
    fn launch_job_watchdog(
        self: Arc<Self>,
        job_id: Uuid,
        supervisor_id: Uuid,
        job_times_out_instant: Instant,
        mut event_channel: UnboundedReceiver<SupervisorJobEvent>,
        confluence_close_tx: watch::Sender<()>,
        stop_rx: oneshot::Receiver<ExitStatus>,
    ) -> (UnboundedReceiver<SupervisorJobEvent>,) {
        let (fwd_tx, fwd_rx) = mpsc::unbounded_channel();
        let span = tracing::info_span!("(job termination watchdog)", %job_id, %supervisor_id);
        let future = async move {
            let sleep = sleep_until(job_times_out_instant).fuse();
            let stop_rx = stop_rx.fuse();

            tokio::pin!(sleep, stop_rx);

            loop {
                tokio::select! {
                    event = event_channel.recv() => {
                        if let Some(sj_event) = event {
                            if let Err(e) = fwd_tx.send(sj_event.clone()) {
                                tracing::error!("Failed to forward supervisor job event: {e:?}");
                            }
                            if matches!(sj_event, SupervisorJobEvent::StateTransition { new_state: RunningJobState::Terminated, .. }) {
                                break
                            }
                        } else {
                            // confluence closed - shouldn't be possible?
                            unreachable!("supervisor job event confluence closed prematurely")
                        }
                    }
                    () = &mut sleep => {
                        tracing::debug!("Hit job timeout");
                        if let Err(e) = sql::job::history::insert(
                            job_id,
                            JobEvent::SetExitStatus {
                                exit_status: ExitStatus::JobTimeout,
                                status_message: None,
                            },
                            Utc::now(),
                            &self.pool
                        ).await {
                            tracing::error!("Failed insert exit status for job ({job_id}) timeout to database: {e:?}");
                            // TODO: double check that state normalization covers this case at
                            //       switchboard startup.
                        }
                        {
                            let mut state = self.state.lock().await;
                            if let Err(e) = self.send_stop_message(job_id, supervisor_id, &mut state).await {
                                tracing::error!("Failed to send supervisor ({supervisor_id}) stop message for job ({job_id}): {e:?}");
                                // force the connection to close
                                break
                            }
                        }
                        continue
                    }
                    // Could either be Ok(()) or Err(oneshot::RecvError); we don't really care which
                    // variant we get. Getting Err would mean the ActiveJob dropped (or rather that
                    // the other end of `stop_rx` dropped), which means something is wrong, and we
                    // really rather ought to close;
                    reason_result = &mut stop_rx => {
                        let exit_reason = match reason_result {
                            Ok(exit_status) => {
                                tracing::debug!("Stop channel received message: {exit_status}");
                                match exit_status {
                                    ExitStatus::JobCanceled => ExitReason::Canceled,
                                    ExitStatus::SupervisorDroppedJob => ExitReason::SupervisorDroppedJob,
                                    invalid_status => {
                                        tracing::error!("termination watchdog for job ({job_id}): stop_rx received bad value: {invalid_status:?}");
                                        ExitReason::Canceled
                                    }
                                }
                            }
                            Err(e) => {
                                // This is more of an internal error situation
                                tracing::error!("termination watchdog for job ({job_id}): stop_rx failed: {e}");
                                ExitReason::Canceled
                            }
                        };
                        if let Err(e) = sql::job::history::insert(
                            job_id,
                            JobEvent::SetExitStatus {
                                exit_status: match exit_reason {
                                    ExitReason::Canceled => ExitStatus::JobCanceled,
                                    ExitReason::SupervisorDroppedJob => ExitStatus::SupervisorDroppedJob
                                },
                                status_message: None,
                            },
                            Utc::now(),
                            &self.pool
                        ).await {
                            tracing::error!("Failed insert exit status for job ({job_id}) {} to database: {e:?}", match exit_reason {
                                ExitReason::Canceled => "cancellation",
                                ExitReason::SupervisorDroppedJob => "dropping",
                            });
                            // TODO: double check that state normalization covers this case at
                            //       switchboard startup.
                        }
                        {
                            let mut state = self.state.lock().await;
                            if let Err(e) = self.send_stop_message(job_id, supervisor_id, &mut state).await {
                                tracing::error!("Failed to send supervisor ({supervisor_id}) stop message for job ({job_id}): {e:?}");
                                // force the connection to close
                                break
                            }
                        }
                        continue
                    }
                };
            }
            // Okay, so, at this point we're confident that either the supervisor has reached the
            // Terminated state, or that there's nothing else we can reasonably do to make it reach
            // that state.
            tracing::debug!("Detected Terminated state, finalizing results");
            let terminated_at = Utc::now();
            struct R {
                job_event: sqlx::types::Json<JobEvent>,
            }
            let events = match sqlx::query_as!(
                R,
                r#"
                select job_event as "job_event: _" from tml_switchboard.job_events
                where job_id = $1
                    and (
                        job_event ->> 'event_type' = 'declare_workload_exit_status'
                     or job_event ->> 'event_type' = 'set_exit_status'
                    )
                order by logged_at asc
                "#,
                job_id
            ).fetch_all(&self.pool).await {
                Ok(evs) => evs,
                Err(e) => {
                    tracing::error!("Failed to fetch exit status events for job ({job_id}) from database: {e:?}");
                    todo!("Internal switchboard error")
                }
            };
            let mut final_status = None::<JobResult>;
            for ev in events.into_iter() {
                match ev.job_event.0 {
                    JobEvent::StateTransition { .. } | JobEvent::FinalizeResult { .. } => {
                        unreachable!()
                    }
                    JobEvent::DeclareWorkloadExitStatus { workload_exit_status, workload_output } => {
                        let exit_status = ExitStatus::from(workload_exit_status);
                        if final_status
                            .as_ref()
                            .map(|fs| matches!(&fs.exit_status,
                                ExitStatus::WorkloadFinishedUnknown
                                | ExitStatus::WorkloadFinishedError
                                | ExitStatus::WorkloadFinishedSuccess))
                            .unwrap_or(true) {
                            final_status = Some(JobResult {
                                job_id,
                                supervisor_id: Some(supervisor_id),
                                exit_status,
                                host_output: workload_output,
                                terminated_at
                            });
                        }
                    }
                    JobEvent::SetExitStatus { exit_status, status_message } => {
                        // FIXME: it is possible that there should be additional restrictions at
                        //        play here.

                        // Restrictions:
                        //  (1) Timeout should not override legitimate declarations of exit status
                        let workload_did_declare = final_status.as_ref()
                            .map(|fs| matches!(&fs.exit_status,
                                ExitStatus::WorkloadFinishedUnknown
                                | ExitStatus::WorkloadFinishedError
                                | ExitStatus::WorkloadFinishedSuccess
                            ))
                            .unwrap_or(false);
                        if workload_did_declare && matches!(exit_status, ExitStatus::JobTimeout) {
                            continue
                        }
                        final_status = Some(JobResult {
                            job_id,
                            supervisor_id: Some(supervisor_id),
                            exit_status,
                            host_output: status_message.map(|msg| format!("message: {msg}")),
                            terminated_at,
                        });
                    }
                }
            }
            let job_result = match final_status {
                Some(fs) => fs,
                None => {
                    // Okay, so no statuses were returned?
                    tracing::error!("Job ({job_id}) has no exit status declarations recorded in the database");
                    JobResult {
                        job_id,
                        supervisor_id: Some(supervisor_id),
                        exit_status: ExitStatus::JobCanceled,
                        host_output: Some("error: no job exit status was recorded".to_string()),
                        terminated_at,
                    }
                }
            };
            tracing::debug!("Final status of job ({job_id}): {} / [{:?}]", job_result.exit_status, job_result.host_output);

            let mut state = self.state.lock().await;
            // Todo: use something nicer than a loop with breaks
            loop {
                let mut tx = match self.pool.begin().await {
                    Ok(tx) => tx,
                    Err(e) => {
                        tracing::error!("Failed to begin transaction: {e:?}");
                        break
                    }
                };
                if let Err(e) = sql_update_jobs_table_for_termination(&job_result, &mut tx).await {
                    tracing::error!("Failed to add query [update jobs table for termination] to transaction: {e:?}");
                    break
                }
                if let Err(e) = sql::job::history::insert(job_id, JobEvent::FinalizeResult { job_result }, terminated_at, tx.as_mut()).await {
                    tracing::error!("Failed to add query [insert finalize event] to transaction: {e:?}");
                    break
                }
                if let Err(e) = tx.commit().await {
                    tracing::error!("Failed to commit termination transaction: {e:?}");
                    break
                }
                break
            }

            tracing::debug!("Removing reservation of supervisor ({supervisor_id})");

            match state.kanban.remove_active_job(job_id) {
                Ok(_) => (),
                Err(e) => {
                    tracing::error!("Failed to remove active job ({job_id}) from kanban: {e:?}");
                }
            };
            if let Err(e) = state.herd.unreserve_supervisor(supervisor_id)  {
                match e {
                    HerdError::NotRegistered => {
                        // this is a problem
                        panic!("failed to unreserve supervisor: not registered")
                    }
                    HerdError::AlreadyRegistered => {
                        // this can't happen
                        unreachable!()
                    }
                    HerdError::NotConnected => {
                        // this is fine
                    }
                    HerdError::InvalidCondition => {
                        // this is fine
                    }
                }
            }

            tracing::debug!("Closing job ({job_id}) event confluence");

            if let Err(e) = confluence_close_tx.send(()) {
                tracing::warn!("Failed to send close event to status confluence: {e:?}");
            }

            drop(fwd_tx);

            drop(state);

            tracing::warn!("Running restart logic (WARNING: RESTART LOGIC NOT YET IMPLEMENTED)");

            // TODO: restart logic
            let should_restart = false;
            if should_restart {
                todo!()
            }

            tracing::info!("Job ({job_id}) termination watchdog closing.");
        }
        .instrument(span);
        tokio::spawn(future);

        (fwd_rx,)
    }

    /// Event recorder is responsible for recording state transitions, and workload exit status
    /// declarations.
    #[instrument(skip(self, forwarded_event_channel))]
    fn launch_event_recorder(
        self: Arc<Self>,
        job_id: Uuid,
        supervisor_id: Uuid,
        mut forwarded_event_channel: UnboundedReceiver<SupervisorJobEvent>,
    ) {
        let span = tracing::info_span!("(job event recorder)", %job_id, %supervisor_id);
        let future = async move {
            loop {
                let Some(event) = forwarded_event_channel.recv().await else {
                    tracing::info!("Forwarded event channel closed, closing event recorder");
                    break;
                };
                match event {
                    SupervisorJobEvent::StateTransition { new_state, status_message } => {
                        tracing::info!("Job ({job_id}) running on ({supervisor_id}) changed execution state to: {new_state:?}");
                        // let workload_did_terminate = matches!(new_state, RunningJobState::Terminated);
                        let as_job_event = JobEvent::StateTransition {
                            state: match new_state {
                                RunningJobState::Initializing { stage } => JobState::Initializing{ stage },
                                RunningJobState::Ready => JobState::Ready,
                                RunningJobState::Terminating => JobState::Terminating,
                                RunningJobState::Terminated => JobState::Terminated,
                            },
                            status_message
                        };
                        if let Err(e) = sql::job::history::insert(
                            job_id,
                            as_job_event,
                            Utc::now(),
                            &self.pool
                        ).await {
                            tracing::error!("Failed to commit state transition of job ({job_id}) running on supervisor ({supervisor_id}) to database: {e:?}");
                        }
                        // // If the job is Terminated, then we can just exit the event recorder now?
                        // XXX(mc): just wait for the termination watchdog to close the message queue
                        // if workload_did_terminate {
                        //     break
                        // }
                    }
                    SupervisorJobEvent::DeclareExitStatus { user_exit_status, host_output } => {
                        tracing::info!("Workload for job ({job_id}) running on ({supervisor_id}) declared exit status: {user_exit_status:?}");
                        if let Err(e) = sql::job::history::insert(
                            job_id,
                            JobEvent::DeclareWorkloadExitStatus { workload_exit_status: user_exit_status, workload_output: host_output },
                            Utc::now(),
                            &self.pool
                        ).await {
                            tracing::error!("Failed to commit exit status {user_exit_status:?} of job ({job_id}) running on supervisor ({supervisor_id}) to database: {e:?}");
                        }
                    }
                    SupervisorJobEvent::Error { error } => {
                        tracing::info!("Received error from job ({job_id}) running on supervisor ({supervisor_id}): {error:?}");
                        // XXX(mc): just wait for teh termination watchdog to close the message queue
                    }
                    SupervisorJobEvent::ConsoleLog { .. } => {
                        tracing::warn!("Received console log from job ({job_id}) running on supervisor ({supervisor_id}), ignoring.");
                        // ignore
                    }
                }
            }
            tracing::info!("Event recorder closing.")
        }
        .instrument(span);
        tokio::spawn(future);
    }

    #[instrument(skip(self, state))]
    async fn send_stop_message(
        &self,
        job_id: Uuid,
        supervisor_id: Uuid,
        // reason: JobResult,
        state: &mut State,
        // send_stop_job_message: bool,
    ) -> Result<(), ServiceError> {
        tracing::debug!("Sending stop job ({job_id}) message to supervisor ({supervisor_id})");
        let active_job = state
            .kanban
            .get_active_job(job_id)
            .ok_or(ServiceError::NoSuchJob)?;
        if let Some(res) = &active_job.current_reservation {
            if let Err(e) = res.stop_job(switchboard_supervisor::StopJobMessage { job_id }) {
                // Not connected, which is actually fine, since we have the State lock
                tracing::warn!("Failed to send stop message, believed disconnected: {e:?}");
            } else {
                tracing::info!(
                    "Successfully sent stop job ({job_id}) message to supervisor ({supervisor_id})"
                );
            }
        } else {
            // Just as it's fine if the message fails to send above, if there's no current
            // reservation, that's also good.
            tracing::warn!("Failed to send stop message: no current reservation");
        }

        Ok(())
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: JOB / SUPERVISOR STATUS
// -------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JobHistoryEntry {
    pub job_id: Uuid,
    pub job_event: JobEvent,
    pub logged_at: DateTime<Utc>,
}
impl From<SqlJobEvent> for JobHistoryEntry {
    fn from(sje: SqlJobEvent) -> Self {
        JobHistoryEntry {
            job_id: sje.job_id,
            job_event: sje.job_event.0,
            logged_at: sje.logged_at,
        }
    }
}
impl Service {
    // pub async fn get_job_status_history(
    //     &self,
    //     job_id: Uuid,
    // ) -> Result<Vec<JobHistoryEntry>, ServiceError> {
    //     let history = sql::job::history::fetch_by_job_id(job_id, &self.pool)
    //         .await
    //         .map_err(ServiceError::Database)?;
    //     Ok(history.into_iter().map(Into::into).collect())
    // }

    // pub async fn get_last_job_status_history(
    //     &self,
    //     job_id: Uuid,
    // ) -> Result<(JobState, Option<String>), ServiceError> {
    //     let history = sql::job::history::fetch_most_recent_state_by_job_id(job_id, &self.pool)
    //         .await
    //         .map_err(ServiceError::Database)?;
    //     history.ok_or(ServiceError::NoSuchJob)
    // }

    pub async fn get_job_status(&self, job_id: Uuid) -> Result<JobStatus, ServiceError> {
        let sql_ssh_endpoints_to_api = |sql_ssh_endpoints: Option<&Vec<SqlSshEndpoint>>| {
            sql_ssh_endpoints.map(|e| {
                e.iter()
                    .map(|sql_ssh_endpoint| JobSshEndpoint {
                        host: sql_ssh_endpoint.ssh_host.clone().into(),
                        port: sql_ssh_endpoint.ssh_port.into(),
                    })
                    .collect()
            })
        };

        let job = sql::job::fetch_by_job_id(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;

        let as_of = Utc::now();

        let finalized_result = sql::job::history::fetch_finalized_result(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;

        Ok(if let Some(result) = finalized_result {
            JobStatus {
                state: ExtendedJobState {
                    state: JobState::Terminated,
                    dispatched_to_supervisor: job.dispatched_on_supervisor_id(),
                    ssh_endpoints: sql_ssh_endpoints_to_api(job.ssh_endpoints()),
                    ssh_user: Some("tml".to_string()), // TODO: determine this from image
                    ssh_host_keys: None,               // TODO: allow supervisor to report host keys
                    result: Some(result),
                },
                as_of,
            }
        } else if let Some((state, _msg)) =
            sql::job::history::fetch_most_recent_state_by_job_id(job_id, &self.pool)
                .await
                .map_err(ServiceError::Database)?
        {
            JobStatus {
                state: ExtendedJobState {
                    state,
                    dispatched_to_supervisor: job.dispatched_on_supervisor_id(),
                    ssh_endpoints: sql_ssh_endpoints_to_api(job.ssh_endpoints()),
                    ssh_user: Some("tml".to_string()), // TODO: determine this from image
                    ssh_host_keys: None,               // TODO: allow supervisor to report host keys
                    result: None,
                },
                as_of,
            }
        } else {
            return Err(ServiceError::NoSuchJob);
        })
    }

    pub async fn get_supervisor_status(
        &self,
        supervisor_id: Uuid,
    ) -> Result<SupervisorStatus, ServiceError> {
        let mut state = self.state.lock().await;
        let reported = state
            .herd
            .get_supervisor_status(supervisor_id)
            .await
            .map(Some)
            .or_else(|e| match e {
                HerdError::NotConnected => Ok(None),
                e => Err(ServiceError::Herd(e)),
            })?;
        tracing::debug!("Supervisor reported status: {reported:?}");
        if let Some(status) = reported {
            match status {
                ReportedSupervisorStatus::OngoingJob { job_id, job_state } => {
                    Ok(SupervisorStatus::Busy { job_id, job_state })
                }
                ReportedSupervisorStatus::Idle => Ok(SupervisorStatus::Idle),
            }
        } else {
            // Supervisor didn't respond, so go off state in database
            if let Some(job_id) = state.kanban.get_active_job_on(supervisor_id) {
                let (job_state, _status_message) =
                    sql::job::history::fetch_most_recent_state_by_job_id(job_id, &self.pool)
                        .await
                        .map_err(ServiceError::Database)?
                        .ok_or(ServiceError::NoSuchJob)?;
                match RunningJobState::try_from(job_state) {
                    Ok(job_state) => Ok(SupervisorStatus::BusyDisconnected { job_id, job_state }),
                    Err(e) => {
                        panic!(
                            "Invalid state: supervisor ({supervisor_id}) is disconnected, but associated job state is: {e:?}"
                        );
                    }
                }
            } else {
                Ok(SupervisorStatus::Disconnected)
            }
        }
    }

    pub async fn list_supervisors(&self) -> Vec<Uuid> {
        let state = self.state.lock().await;
        state.herd.list_supervisors()
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: UTILITY FUNCTIONS
// -------------------------------------------------------------------------------------------------

// Conservative approximate conversion.
fn datetime_to_instant_approx(datetime: DateTime<Utc>) -> Result<Instant, chrono::OutOfRangeError> {
    let instant_now = Instant::now();
    let datetime_now = Utc::now();
    let timedelta = datetime - datetime_now;
    Ok(instant_now + timedelta.to_std()?)
}

/// Used internally in the job termination system.
#[derive(Debug, Clone)]
enum ExitReason {
    Canceled,
    SupervisorDroppedJob,
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: SQL HELPERS
// -------------------------------------------------------------------------------------------------

async fn sql_update_jobs_table_for_termination(
    job_result: &JobResult,
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set
            functional_state = 'finalized',
            exit_status = $2,
            host_output = $3,
            terminated_at = $4,
            last_updated_at = default
        where job_id = $1;
        "#,
        job_result.job_id,
        SqlExitStatus::from(job_result.exit_status) as SqlExitStatus,
        job_result.host_output.clone(),
        job_result.terminated_at,
    )
    .execute(tx.as_mut())
    .await?;

    Ok(())
}

async fn sql_set_job_state_to_scheduled(
    started_at: DateTime<Utc>,
    job_id: Uuid,
    supervisor_id: Uuid,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set
            functional_state = 'dispatched',
            started_at = $1,
            dispatched_on_supervisor_id = $2,
            ssh_endpoints = (
              select ssh_endpoints from tml_switchboard.supervisors
              where supervisor_id = $2
            ),
            last_updated_at = default
        where job_id = $3;"#,
        started_at,
        supervisor_id,
        job_id
    )
    .execute(tx.as_mut())
    .await?;
    sql::job::history::insert(
        job_id,
        JobEvent::StateTransition {
            state: JobState::Scheduled,
            status_message: None,
        },
        started_at,
        tx.as_mut(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
//
// async fn sql_set_job_state_to_running(
//     started_at: DateTime<Utc>,
//     job_id: Uuid,
//     supervisor_id: Uuid,
//     conn: impl PgExecutor<'_>,
// ) -> Result<(), sqlx::Error> {
//     Ok(())
// }

#[allow(dead_code)]
async fn sql_fetch_running_jobs_by_supervisor_id(
    supervisor_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query!(
        r#"
        select job_id from tml_switchboard.jobs
        where functional_state = 'dispatched' and dispatched_on_supervisor_id = $1;
        "#,
        supervisor_id
    )
    .fetch_all(conn)
    .await
    .map(|v| v.into_iter().map(|record| record.job_id).collect())
}

async fn sql_enforce_queue_timeout(job_id: Uuid, pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let terminated_at = Utc::now();
    let exit_status = ExitStatus::QueueTimeout;
    let job_result = JobResult {
        job_id,
        supervisor_id: None,
        exit_status,
        host_output: None,
        terminated_at,
    };
    sql_update_jobs_table_for_termination(&job_result, &mut tx).await?;
    sql::job::history::insert(
        job_id,
        JobEvent::SetExitStatus {
            exit_status,
            status_message: None,
        },
        terminated_at,
        tx.as_mut(),
    )
    .await?;
    sql::job::history::insert(
        job_id,
        JobEvent::FinalizeResult { job_result },
        terminated_at,
        tx.as_mut(),
    )
    .await?;

    tx.commit().await?;

    Ok(())
}

async fn sql_stop_orphaned_job(job_id: Uuid, pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let terminated_at = Utc::now();
    let exit_status = ExitStatus::SupervisorMatchError;
    let job_result = JobResult {
        job_id,
        supervisor_id: None,
        exit_status,
        host_output: None,
        terminated_at,
    };
    sql_update_jobs_table_for_termination(&job_result, &mut tx).await?;
    sql::job::history::insert(
        job_id,
        JobEvent::SetExitStatus {
            exit_status,
            status_message: None,
        },
        terminated_at,
        tx.as_mut(),
    )
    .await?;
    sql::job::history::insert(
        job_id,
        JobEvent::FinalizeResult { job_result },
        terminated_at,
        tx.as_mut(),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

async fn sql_build_job_restart(
    new_job_id: Uuid,
    original: SqlJob,
    queued_at: DateTime<Utc>,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"
        insert into tml_switchboard.job_parameters
        select $1, key, value
        from tml_switchboard.job_parameters
        where job_id = $2;
        "#,
        new_job_id,
        original.job_id(),
    )
    .execute(tx.as_mut())
    .await?;
    let ImageSpecification::Image { image_id } = original.read_image_spec() else {
        unreachable!()
    };
    let sql_restart_policy = SqlRestartPolicy {
        remaining_restart_count: original
            .restart_policy()
            .remaining_restart_count
            .saturating_sub(1)
            .try_into()
            .expect("remaining restart count >= i32::MAX (impossible)"),
    };
    sqlx::query!(
        r#"
        insert into tml_switchboard.jobs
        (
          job_id,
          resume_job_id,
          restart_job_id,
          image_id,
          ssh_keys,
          restart_policy,
          enqueued_by_token_id,
          tag_config,
          job_timeout,
          functional_state,
          queued_at,
          started_at,
          dispatched_on_supervisor_id,
          ssh_endpoints,
          exit_status,
          host_output,
          terminated_at,
          last_updated_at
        )
        values
        (
          $1,       -- job_id
          null,	    -- resume_job_id
          $2,	    -- restart_job_id
          $3,	    -- image_id
          $4,	    -- ssh_keys
          $5,	    -- restart_policy
          $6,	    -- enqueued_by_token_id
          $7,	    -- tag_config
          $8,	    -- job_timeout
          'queued', -- functional_state
          $9,	    -- queued_at
          null,	    -- started_at
          null,	    -- dispatched_on_supervisor_id
          null,	    -- ssh_endpoints
          null,	    -- exit_status
          null,	    -- host_output
          null,	    -- terminated_at
          default   -- last_updated_at
        )
        "#,
        new_job_id,
        original.job_id(),
        image_id.as_bytes(),
        original.ssh_keys(),
        sql_restart_policy as SqlRestartPolicy,
        original.enqueued_by_token_id(),
        original.raw_tag_config(),
        PgInterval::try_from(original.timeout())
            .expect("failed to restore to PgInterval from TimeDelta intermediate"),
        queued_at,
    )
    .execute(tx.as_mut())
    .await?;
    sql::job::history::insert(
        new_job_id,
        JobEvent::StateTransition {
            state: JobState::Queued,
            status_message: Some(format!("Restarting job {}", original.job_id())),
        },
        queued_at,
        tx.as_mut(),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

async fn sql_register_job(
    job_request: JobRequest,
    job_id: Uuid,
    as_token_id: Uuid,
    job_timeout: PgInterval,
    queued_at: DateTime<Utc>,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let params = job_request.parameters.clone();
    sql::job::insert(
        job_request,
        job_id,
        as_token_id,
        job_timeout,
        queued_at,
        &mut tx,
    )
    .await?;
    sql::job::parameters::insert(job_id, params, tx.as_mut()).await?;
    sql::job::history::insert(
        job_id,
        JobEvent::StateTransition {
            state: JobState::Queued,
            status_message: None,
        },
        queued_at,
        tx.as_mut(),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}
