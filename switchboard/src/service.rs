use crate::auth::db::DbAuth;
use crate::auth::{AuthorizationSource, Privilege};
use crate::config::ServiceConfig;
use crate::perms::RunJobOnSupervisor;
use crate::service::herd::{Herd, HerdError, ReservationError};
use crate::service::kanban::{Kanban, KanbanError};
use crate::service::tag::NaiveTagConfig;
use crate::sql;
use crate::sql::api_token::TokenError;
use crate::sql::job::history::SqlJobHistoryEntry;
use crate::sql::job::{SqlExitStatus, SqlJob, SqlRestartPolicy, SqlSimpleState};
use axum::extract::ws::WebSocket;
use chrono::{DateTime, TimeDelta, Utc};
use futures_util::stream::FuturesOrdered;
use futures_util::StreamExt;
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, PgPool, Postgres};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};
use tracing::{instrument, Instrument};
use treadmill_rs::api::switchboard::{
    ExitStatus, JobRequest, JobResult, JobStatus, SupervisorStatus,
};
use treadmill_rs::api::switchboard_supervisor;
use treadmill_rs::api::switchboard_supervisor::{ImageSpecification, ReportedSupervisorStatus};
use treadmill_rs::connector::{JobError, JobState};
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
    FailedToMatch,
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
                        let mut state = self.state.lock().await;
                        let idle_set = state.herd.currently_idle();
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

        let active_jobs = sql::job::fetch_all_running(&self.pool)
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
                sql_finish_job(
                    JobResult {
                        job_id: job.job_id(),
                        supervisor_id: None,
                        exit_status: ExitStatus::HostTerminatedTimeout,
                        host_output: None,
                        terminated_at: Utc::now(),
                    },
                    &mut tx,
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
            let supervisor_id = job.running_on_supervisor_id().expect(
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

            let jsr = launch_job_status_tee(self.pool.clone(), job.job_id(), jsr);

            // XXX(max): I think this isn't actually necessary here; it won't produce a deadlock in
            // launch_termination_watchdog, just force it to wait for a bit extra.
            // drop(state);

            Arc::clone(self).launch_termination_watchdog(
                job.job_id(),
                supervisor_id,
                job_times_out_at_instant,
                jsr,
                close_tx,
                stop_rx,
            );
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

        tracing::info!("Restarted {restarted_count}/{restartable_count} (eligible)/{total_count} (expired) jobs.");

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
            herd::prepare_supervisor_connection(supervisor_id, socket);

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
                        tracing::error!("Job mismatch: expected job ({job_id}), found running job ({running_job_id}).");
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
                        state
                            .herd
                            .supervisor_connected(supervisor_id, connected_supervisor)
                            .map_err(ServiceError::Herd)?;
                    } else {
                        tracing::info!("Detection successful: job ({job_id}) is running on supervisor ({supervisor_id}).");
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
                            panic!("System in disorder: JSR disconnected on active job with state locked");
                        }
                        // TODO: resolve A-B-A reservation problem when disconnect gets processed after connect
                        maybe_active_job.current_reservation = Some(reservation);
                    }
                }
                ReportedSupervisorStatus::Idle => {
                    // Oops
                    tracing::warn!("Detected job drop: supervisor ({supervisor_id}) is no longer reporting job ({job_id}) as active.");
                    let active_job = state.kanban.get_active_job(job_id).unwrap();
                    if let Some(tx) = active_job.stop_tx.take() {
                        let _ = tx.send(ExitStatus::HostDroppedJob);
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
                    return Err(ServiceError::FailedToMatch);
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
            let mut tx = self.pool.begin().await.map_err(ServiceError::Database)?;
            sql_finish_job(
                JobResult {
                    job_id,
                    supervisor_id: None,
                    exit_status: ExitStatus::FailedToMatch,
                    host_output: None,
                    terminated_at: Utc::now(),
                },
                &mut tx,
            )
            .await
            .map_err(ServiceError::Database)?;
            tx.commit().await.map_err(ServiceError::Database)?;

            return Err(ServiceError::FailedToMatch);
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
        match sql_job.simple_state() {
            SqlSimpleState::Inactive => {
                // Simple case: job is already inactive
                return Ok(());
            }
            SqlSimpleState::Queued => {
                // Slightly more complicated: job is in the queue
                let queued_job = state
                    .kanban
                    .remove_queued_job(job_id)
                    .map_err(ServiceError::Kanban)?;
                // Disarm the queue timeout watchdog
                let _ = queued_job.exit_notifier.send(());
                let mut tx = self.pool.begin().await.map_err(ServiceError::Database)?;
                sql_finish_job(
                    JobResult {
                        job_id,
                        supervisor_id: None,
                        exit_status: ExitStatus::JobCanceled,
                        host_output: None,
                        terminated_at: Utc::now(),
                    },
                    &mut tx,
                )
                .await
                .map_err(ServiceError::Database)?;
                tx.commit().await.map_err(ServiceError::Database)?;
            }
            SqlSimpleState::Running => {
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
            ssh_rendezvous_servers: self.service_config.ssh.rendezvous_servers.clone(),
            parameters: sql_params,
        };

        let mut state = self.state.lock().await;

        tracing::debug!("Running job setup");

        if !state.kanban.queued_job_exists(job_id) {
            return Err(ServiceError::NoLongerQueued);
        }

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

        if let Err(e) = sql_set_job_state_to_running(started_at, job_id, supervisor_id, &self.pool)
            .await
            .map_err(ServiceError::Database)
        {
            tracing::error!("Failed to update job state in database: {e}, retracting stopping job and reservation.");

            if let Err(e) = reservation.stop_job(switchboard_supervisor::StopJobMessage { job_id })
            {
                // not connected, which is... fine?
                tracing::warn!("Unable to send job stop message: {e}");
            } else {
                tracing::debug!("Successfully sent job stop message");
            }

            drop(reservation);
            // possible errors: not connected, not reserved, not registered.
            // in all these cases, the supervisor is no longer considered as "reserved", therefore
            // no error shall be returned.
            if let Err(e) = state.herd.unreserve_supervisor(supervisor_id) {
                tracing::warn!("Failed to unreserve supervisor ({supervisor_id}): {e}");
            }

            tracing::debug!("Successfully retracted launch attempt");

            return Err(e);
        }

        tracing::debug!("Successfully updated job state in database");

        let (stop_tx, stop_rx) = oneshot::channel::<ExitStatus>();

        let (jsr, close_tx) = state
            .kanban
            .activate(job_id, supervisor_id, reservation, stop_tx)
            .expect("activation failure, despite job existing and being queued");

        tracing::debug!("Successfully activated job ({job_id})");

        tracing::debug!("Launching job status tee for job ({job_id})");

        // Upload job status events to the database, then forward them to `jsr`.
        let jsr = launch_job_status_tee(self.pool.clone(), job_id, jsr);

        // Unlock the state mutex as early as possible.
        drop(state);

        // Launch termination watchdog
        Arc::clone(self).launch_termination_watchdog(
            job_id,
            supervisor_id,
            job_times_out_instant,
            jsr,
            close_tx,
            stop_rx,
        );

        tracing::debug!("Launching termination watchdog for job ({job_id})");

        Ok(())
    }

    #[instrument(skip(self, state))]
    async fn stop_internal(
        &self,
        job_id: Uuid,
        supervisor_id: Uuid,
        reason: JobResult,
        state: &mut State,
        send_stop_job_message: bool,
    ) -> Result<(), ServiceError> {
        // update info in database
        // remove job from kanban
        // close reservation
        // (try to) unreserve supervisor

        let mut tx = self.pool.begin().await.map_err(ServiceError::Database)?;
        sql_finish_job(reason, &mut tx)
            .await
            // TODO: what to do if this fails?
            .map_err(ServiceError::Database)?;
        tx.commit().await.map_err(ServiceError::Database)?;

        let active_job = state
            .kanban
            .remove_active_job(job_id)
            .map_err(ServiceError::Kanban)?;
        if send_stop_job_message {
            if let Some(res) = active_job.current_reservation {
                if let Err(_e) = res
                    .stop_job(switchboard_supervisor::StopJobMessage { job_id })
                    .map_err(ServiceError::Reservation)
                {
                    // not connected, which is... fine?
                    tracing::warn!("Failed to send stop message, believed disconnected");
                }
            } else {
                tracing::warn!("Can't send stop message: no current reservation");
            }
        }
        if let Err(e) = state.herd.unreserve_supervisor(supervisor_id) {
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

        Ok(())
    }

    fn launch_termination_watchdog(
        self: Arc<Self>,
        job_id: Uuid,
        supervisor_id: Uuid,
        job_times_out_instant: Instant,
        mut jsr: UnboundedReceiver<JobStatus>,
        close_tx: watch::Sender<()>,
        mut stop_rx: oneshot::Receiver<ExitStatus>,
    ) {
        // can't #[instrument] a closure, so
        let span = tracing::info_span!("(job termination watchdog)", %job_id, %supervisor_id);
        let termination_watchdog_future = async move {
            let sleep = sleep_until(job_times_out_instant);
            // Needed to `tokio::select!` on `sleep` in a loop
            tokio::pin!(sleep);

            // Wait until job times out OR receives a termination status.
            let exit_reason = loop {
                let job_status = tokio::select! {
                    event = jsr.recv() => {
                        // received Option<JobStatus>
                        if let Some(status) = event {
                            status
                        } else {
                            // connection closed, which, uh, should only happen if JSR closes or
                            // close_tx goes off, so in theory not possible here?
                            unreachable!()
                        }
                    },
                    () = &mut sleep => {
                        // timeout
                        break ExitReason::Timeout
                    },
                    // could either be Ok(()) or Err(oneshot::RecvError); we don't really care which
                    // Err variant means the ActiveJob dropped or someone did a `mem::swap` or
                    // something similar so /shrug
                    reason = &mut stop_rx => {
                        break match reason {
                            Ok(reason) => match reason {
                                ExitStatus::JobCanceled => ExitReason::Canceled,
                                ExitStatus::HostDroppedJob => ExitReason::HostDropped,
                                x => {
                                    tracing::error!("termination watchdog for job ({job_id}): stop_rx received bad value: {x:?}");
                                    ExitReason::Canceled
                                }
                            }
                            Err(e) => {
                                tracing::error!("termination watchdog for job ({job_id}): stop_rx failed: {e}");
                                ExitReason::Canceled
                            }
                        }
                    }
                };
                match job_status {
                    JobStatus::Active {
                        job_state: JobState::Finished { status_message },
                    } => break ExitReason::Finished { status_message },
                    JobStatus::Active {
                        job_state: JobState::Canceled,
                    } => break ExitReason::Canceled,
                    JobStatus::Error { job_error } => break ExitReason::Error { error: job_error },
                    JobStatus::Active { .. } => continue,
                    JobStatus::Inactive => continue,
                    JobStatus::Terminated(_) => {
                        unreachable!()
                    }
                }
            };

            tracing::info!("Job ({job_id}) terminated with reason: {exit_reason:?}");

            // Actually, we always want to send a stop job message
            //let send_stop_message = matches!(&exit_reason, ExitReason::Timeout);

            let job_result = build_job_result(job_id, supervisor_id, exit_reason);

            let mut state = self.state.lock().await;
            if let Err(e) = self
                .stop_internal(
                    job_id,
                    supervisor_id,
                    job_result,
                    &mut state,
                    true,
                )
                .await
            {
                // uh okay not much we can do tbh
                tracing::error!("Failed to stop terminated job: {e}");
            } else {
                tracing::debug!("Successfully stopped job ({job_id})");
            }

            // Close the JSR reloader and the job status tee
            if let Err(e) = close_tx.send(()) {
                tracing::warn!("Failed to send close event to status confluence: {e}");
            }

            // is there anything else we need to do?
            // TODO: restart logic

            tracing::info!("Finished termination process for job ({job_id})");
        }
            .instrument(span);

        tokio::spawn(termination_watchdog_future);
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: JOB / SUPERVISOR STATUS
// -------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JobHistoryEntry {
    pub job_id: Uuid,
    pub job_state: JobStatus,
    pub logged_at: DateTime<Utc>,
}
impl From<SqlJobHistoryEntry> for JobHistoryEntry {
    fn from(sjhe: SqlJobHistoryEntry) -> Self {
        JobHistoryEntry {
            job_id: sjhe.job_id,
            job_state: sjhe.job_state.0,
            logged_at: sjhe.logged_at,
        }
    }
}
impl Service {
    pub async fn get_job_status_history(
        &self,
        job_id: Uuid,
    ) -> Result<Vec<JobHistoryEntry>, ServiceError> {
        let history = sql::job::history::fetch_by_job_id(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;
        Ok(history.into_iter().map(Into::into).collect())
    }

    pub async fn get_last_job_status_history(
        &self,
        job_id: Uuid,
    ) -> Result<JobHistoryEntry, ServiceError> {
        let history = sql::job::history::fetch_most_recent_by_job_id(job_id, &self.pool)
            .await
            .map_err(ServiceError::Database)?;
        history.map(Into::into).ok_or(ServiceError::NoSuchJob)
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
        tracing::debug!("reported: {reported:?}");
        if let Some(status) = reported {
            match status {
                ReportedSupervisorStatus::OngoingJob { job_id, job_state } => {
                    Ok(SupervisorStatus::Busy { job_id, job_state })
                }
                ReportedSupervisorStatus::Idle => Ok(SupervisorStatus::Idle),
            }
        } else {
            if let Some(job_id) = state.kanban.get_active_job_on(supervisor_id) {
                let job_status_history_entry = self.get_last_job_status_history(job_id).await?;
                match job_status_history_entry.job_state {
                    JobStatus::Active { job_state } => {
                        Ok(SupervisorStatus::BusyDisconnected { job_id, job_state })
                    }
                    _ => panic!("Temporal constraint violation. Please reroute coolant to temporal dynamo in sector 34L-X"),
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

#[derive(Debug, Clone)]
enum ExitReason {
    Finished { status_message: Option<String> },
    Canceled,
    Error { error: JobError },
    Timeout,
    HostDropped,
}

fn build_job_result(job_id: Uuid, supervisor_id: Uuid, exit_reason: ExitReason) -> JobResult {
    match exit_reason {
        ExitReason::Finished { status_message } => {
            let host_output = status_message.map(|s| {
                serde_json::from_str(&s).unwrap_or_else(|e| {
                    let x = format!("failed to parse host output: {e}");
                    serde_json::json!({"error": x})
                })
            });
            JobResult {
                job_id,
                supervisor_id: Some(supervisor_id),
                exit_status: ExitStatus::HostTerminatedWithSuccess,
                host_output,
                terminated_at: Utc::now(),
            }
        }
        ExitReason::Canceled => JobResult {
            job_id,
            supervisor_id: Some(supervisor_id),
            exit_status: ExitStatus::JobCanceled,
            host_output: None,
            terminated_at: Utc::now(),
        },
        ExitReason::Error { error } => {
            let host_output = serde_json::to_value(error).unwrap_or_else(|e| {
                let x = format!("failed to serialize host error: {e}");
                serde_json::json!({"error": x})
            });
            JobResult {
                job_id,
                supervisor_id: Some(supervisor_id),
                exit_status: ExitStatus::HostTerminatedWithError,
                host_output: Some(host_output),
                terminated_at: Utc::now(),
            }
        }
        ExitReason::Timeout => JobResult {
            job_id,
            supervisor_id: Some(supervisor_id),
            exit_status: ExitStatus::HostTerminatedTimeout,
            host_output: None,
            terminated_at: Utc::now(),
        },
        ExitReason::HostDropped => JobResult {
            job_id,
            supervisor_id: Some(supervisor_id),
            exit_status: ExitStatus::HostDroppedJob,
            host_output: None,
            terminated_at: Utc::now(),
        },
    }
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: WATCHDOG LAUNCHERS
// -------------------------------------------------------------------------------------------------

fn launch_job_status_tee(
    pool: PgPool,
    job_id: Uuid,
    mut jsr: UnboundedReceiver<JobStatus>,
) -> UnboundedReceiver<JobStatus> {
    let (tee_tx, tee_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = jsr.recv().await {
            let _ = sql::job::history::insert(job_id, event.clone(), Utc::now(), &pool).await;
            let _ = tee_tx.send(event);
        }
    });
    tee_rx
}

// -------------------------------------------------------------------------------------------------
// === GROUPING: SQL HELPERS
// -------------------------------------------------------------------------------------------------

async fn sql_finish_job(
    job_result: JobResult,
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"update tml_switchboard.jobs
            set simple_state = 'inactive'
            where job_id = $1;"#,
        job_result.job_id,
    )
    .execute(tx.as_mut())
    .await?;
    sqlx::query!(
        r#"insert into tml_switchboard.job_results
            values ($1, $2, $3, $4, $5);"#,
        job_result.job_id,
        job_result.supervisor_id,
        SqlExitStatus::from(job_result.exit_status) as SqlExitStatus,
        job_result.host_output,
        job_result.terminated_at
    )
    .execute(tx.as_mut())
    .await?;
    let job_id = job_result.job_id;
    sql::job::history::insert(
        job_id,
        JobStatus::Terminated(job_result),
        Utc::now(),
        tx.as_mut(),
    )
    .await?;

    Ok(())
}

async fn sql_set_job_state_to_running(
    started_at: DateTime<Utc>,
    job_id: Uuid,
    supervisor_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"update tml_switchboard.jobs
           set simple_state = 'running', started_at = $1, running_on_supervisor_id = $2
           where job_id = $3;"#,
        started_at,
        supervisor_id,
        job_id
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn sql_fetch_running_jobs_by_supervisor_id(
    supervisor_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query!(
        r#"
        select job_id from tml_switchboard.jobs
        where simple_state = 'running' and running_on_supervisor_id = $1;
        "#,
        supervisor_id
    )
    .fetch_all(conn)
    .await
    .map(|v| v.into_iter().map(|record| record.job_id).collect())
}

async fn sql_enforce_queue_timeout(job_id: Uuid, pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sql_finish_job(
        JobResult {
            job_id,
            supervisor_id: None,
            exit_status: ExitStatus::QueueTimeout,
            host_output: None,
            terminated_at: Utc::now(),
        },
        &mut tx,
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

async fn sql_stop_orphaned_job(job_id: Uuid, pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sql_finish_job(
        JobResult {
            job_id,
            supervisor_id: None,
            exit_status: ExitStatus::FailedToMatch,
            host_output: None,
            terminated_at: Utc::now(),
        },
        &mut tx,
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
                values ($1, null, $2, $3, $4, $5, $6, $7, $8, $9, 'queued', null, null)
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
    sql::job::insert(
        job_request,
        job_id,
        as_token_id,
        job_timeout,
        queued_at,
        &mut tx,
    )
    .await?;
    sql::job::history::insert(job_id, JobStatus::Inactive, queued_at, tx.as_mut()).await?;
    tx.commit().await?;

    Ok(())
}
