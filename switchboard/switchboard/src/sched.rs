use crate::herd::{Herd, HerdError, HerdIter};
use crate::jobs::{build_start_message, JobError, Kanban, KanbanError};
use crate::model::job::{ExitStatus, JobModel};
use crate::server::auth::DbPermSource;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::Timeout;
use treadmill_rs::api::switchboard::JobStatus;
use treadmill_rs::api::switchboard_supervisor::{JobInitSpec, ParameterValue, RestartPolicy};
use treadmill_rs::connector::{JobErrorKind, JobState};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum SchedError {
    #[error("kanban error: {0}")]
    Kanban(KanbanError),
    #[error("herd error: {0}")]
    Herd(HerdError),
    #[error("job error: {0}")]
    Job(JobError),
    // Duplicates?
    #[error("failed to send message to supervisor")]
    FailedToSend,
    // Not duplicates
    #[error("invalid job timeout")]
    InvalidTimeout,
    #[error("database error: {0}")]
    Database(sqlx::Error),
}

struct JobMonitor {
    /// Used for job restart watchdog
    timed_termination_future: Timeout<JoinHandle<TerminationStatus>>,
    /// For convenience, in the job restart watchdog
    #[allow(dead_code)]
    // TODO: make it so that instead of having to lock_metadata() in launch_with_watchdog, we can
    //       just pass all the necessary information through the JobMonitor.
    restart_policy: RestartPolicy,
}
enum TerminationStatus {
    // corresponds to host_terminated_with_success
    Finished {
        #[allow(dead_code)]
        status_message: Option<String>,
    },
    // corresponds to host_terminated_with_error
    Error {
        #[allow(dead_code)]
        error_kind: JobErrorKind,
        #[allow(dead_code)]
        description: String,
    },
    // corresponds to job_canceled
    Canceled,
}
fn wait_for_termination(
    mut job_status_watch: watch::Receiver<JobStatus>,
    timeout: std::time::Duration,
) -> Timeout<JoinHandle<TerminationStatus>> {
    let termination_future = tokio::spawn(async move {
        loop {
            let final_status = job_status_watch
                .wait_for(|js| match &*js {
                    JobStatus::Active { job_state } => match job_state {
                        JobState::Finished { .. } | JobState::Canceled => true,
                        _ => false,
                    },
                    JobStatus::Error { .. } => true,
                    JobStatus::Inactive => false,
                })
                .await;
            match final_status {
                Err(_) => {
                    // watch::Sender dropped, which means that either the
                    // UnboundedReceiver<SupervisorEvent> owned by the SupervisorActorProxy dropped,
                    // or the underlying UnboundedSender<SupervisorEvent> (event_report_queue) owned
                    // by the SupervisorConnector dropped.
                    // In either case, it indicates that the supervisor disconnected; however,
                    // switchboard will ignore disconnected supervisors for job purposes. The only
                    // things that can cancel an active job are user cancellation and timeout.
                    continue;
                }
                Ok(actual_final_status) => {
                    return match &*actual_final_status {
                        JobStatus::Active {
                            job_state: JobState::Finished { status_message },
                        } => TerminationStatus::Finished {
                            status_message: status_message.clone(),
                        },
                        JobStatus::Active {
                            job_state: JobState::Canceled,
                        } => TerminationStatus::Canceled,
                        JobStatus::Error { job_error } => TerminationStatus::Error {
                            error_kind: job_error.error_kind.clone(),
                            description: job_error.description.clone(),
                        },
                        _ => {
                            unreachable!();
                        }
                    }
                }
            };
        }
    });
    let boxed_termination_future = termination_future;
    let timed_termination_future = tokio::time::timeout(timeout, boxed_termination_future);
    timed_termination_future
}

async fn catalyze(
    herd: &Arc<Herd>,
    kanban: &Arc<Kanban>,
    job_id: Uuid,
    supervisor_id: Uuid,
    db: &PgPool,
) -> Result<JobMonitor, SchedError> {
    // lock the job info
    let mut job_lock = kanban
        .lock_metadata(job_id)
        .await
        .map_err(SchedError::Kanban)?;
    tracing::debug!("C1");
    // reserve the actor proxy for the supervisor
    let supervisor_actor_proxy = herd
        .reserve_proxy(supervisor_id)
        .await
        .map_err(SchedError::Herd)?;
    tracing::debug!("C2");
    let timeout = job_lock
        .job_spec()
        .timeout()
        .to_std()
        .map_err(|_| SchedError::InvalidTimeout)?;
    // build a StartJobMessage to send to the supervisor
    let start_message = build_start_message(job_lock.job_spec(), db)
        .await
        .map_err(SchedError::Database)?;
    tracing::debug!("C3");
    // tell the supervisor to start the job
    if supervisor_actor_proxy
        .start_job(start_message)
        .await
        .is_none()
    {
        return Err(SchedError::FailedToSend);
    }
    tracing::debug!("C4");
    // set the job as active
    let active = job_lock
        .activate(supervisor_actor_proxy)
        .map_err(SchedError::Job)?;
    // create a future that will complete on timeout or job termination
    let timed_termination_future = wait_for_termination(active.watch_status(), timeout);
    // update the information in the database:
    let _ = sqlx::query!(
        r#"
        update jobs
        set known_state = 'running', started_at = current_timestamp
        where job_id = $1;"#,
        job_id,
    )
    .execute(db)
    .await
    .map_err(SchedError::Database)?;
    tracing::debug!("C5");
    // create the termination timer
    Ok(JobMonitor {
        timed_termination_future,
        restart_policy: job_lock.job_spec().restart_policy().clone(),
    })
    // Is there anything else that needs to be done?
}

fn config_matches(tag_config: &String, tags: &[String]) -> bool {
    // TODO: replace with something better
    tags.contains(tag_config)
}

#[derive(Debug)]
pub struct Scheduler {
    herd: Arc<Herd>,
    kanban: Arc<Kanban>,
    db: PgPool,
    queue_lock: Mutex<()>,
}
impl Scheduler {
    pub async fn with_database(db: PgPool) -> Result<Self, sqlx::Error> {
        let herd = Herd::from_database(&db).await?;
        Ok(Self {
            herd: Arc::new(herd),
            kanban: Arc::new(Kanban::new()),
            db,
            queue_lock: Mutex::default(),
        })
    }

    pub fn herd(&self) -> &Arc<Herd> {
        &self.herd
    }

    pub async fn enqueue(
        &self,
        job_model: JobModel,
        params: HashMap<String, ParameterValue>,
        auth_source: DbPermSource,
    ) -> Result<(), SchedError> {
        // perms::jobs will handle the database stuff, so we just have to handle queuing
        self.kanban
            .register(job_model, params, auth_source)
            .map_err(SchedError::Kanban)
    }

    async fn queue_poll(self: &Arc<Self>) {
        let mut jobs = vec![];
        // for all jobs, find a supervisor that matches.
        for job in self.kanban.jobs() {
            let (&job_id, job_actor) = job.pair();
            if let Ok(job_actor) = job_actor.try_lock() {
                if !job_actor.is_active() {
                    jobs.push((job_id, job_actor.job_spec().tag_config().to_owned()))
                }
            }
        }
        let dd_jobs = jobs
            .iter()
            .map(|(id, _)| format!("{id}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut match_set = vec![];
        let mut d_supervisors = vec![];
        {
            let st = self.herd.lock_supervisors().await;
            let super_iter = HerdIter::new(&st);
            for (supervisor_id, tags) in super_iter {
                d_supervisors.push(supervisor_id);
                let mut took_job_index = None;
                for (i, (job_id, tag_config)) in jobs.iter().enumerate() {
                    if config_matches(tag_config, tags) {
                        match_set.push((*job_id, supervisor_id));
                        took_job_index = Some(i);
                        break;
                    }
                }
                if let Some(job_index) = took_job_index {
                    jobs.remove(job_index);
                }
            }
        }
        for (job_id, supervisor_id) in match_set {
            if let Err(e) = self.launch_with_watchdog(job_id, supervisor_id).await {
                tracing::error!(
                    "Failed to launch job ({job_id}) on supervisor ({supervisor_id}): {e}"
                );
                panic!("no failure management strategy is set at present.");
            }
            tracing::debug!("Launched job ({job_id}) on supervisor ({supervisor_id})");
        }
        let dd_supervisors = d_supervisors
            .iter()
            .map(|id| format!("{id}"))
            .collect::<Vec<_>>()
            .join(" ");

        tracing::debug!("Checked queue:\n\tJobs: {dd_jobs}\n\tSupervisors: {dd_supervisors}");
    }

    pub fn launch_queue_watchdog(self: &Arc<Self>) {
        let this1 = Arc::clone(self);
        tokio::spawn(async move {
            while let Err(e) = {
                let this = Arc::clone(&this1);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                        let lg = this.queue_lock.lock().await;
                        this.queue_poll().await;
                        let _ = lg;
                    }
                })
                .await
            } {
                tracing::error!("queue watchdog panicked: {e}. restarting.");
            }
        });
    }

    /// Launch the specified job on the specified supervisor, and additionally created a watchdog
    /// task that will wait for the job to terminate, and then both remove it from the kanban, and
    /// unreserve the supervisor.
    async fn launch_with_watchdog(
        self: &Arc<Self>,
        job_id: Uuid,
        supervisor_id: Uuid,
    ) -> Result<(), SchedError> {
        tracing::debug!("Launching job ({job_id}) on supervisor ({supervisor_id})");
        // Needed in the watchdog
        let (restart_policy, init_spec) = {
            let job_actor = self
                .kanban
                .lock_metadata(job_id)
                .await
                .map_err(SchedError::Kanban)?;
            (
                job_actor.job_spec().restart_policy().clone(),
                job_actor.job_spec().init_spec().clone(),
            )
        };

        tracing::debug!("RPIS");

        // catalyze() does the `launch` part of `launch_with_watchdog`.
        let job_monitor =
            catalyze(&self.herd, &self.kanban, job_id, supervisor_id, &self.db).await?;

        tracing::debug!("JM");

        // Spawn a watchdog that waits until the job terminates or times out, and then carries out
        // cleanup.
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // The watchdog waits for the job to terminate, at which point it will run some cleanup.

            // First, we must wait for the job to terminate. We do this using the
            //
            //      Job.timed_termination_future
            //
            // that catalyze() produces.
            let (needs_restart, needs_cancel) = match job_monitor.timed_termination_future.await {
                Ok(Ok(termination_status)) => match termination_status {
                    TerminationStatus::Error { .. } => (true, false),
                    TerminationStatus::Finished { .. } | TerminationStatus::Canceled => {
                        (false, false)
                    }
                },
                // failed to join task; specifically, a panic occurred
                Ok(Err(join_error)) => {
                    tracing::error!("failed to join monitor: panicked: {join_error}");
                    (true, false)
                }
                // hit timeout, need to cancel job
                Err(_elapsed) => (false, true),
            };

            // Run any cleanup that may be necessary after job termination.

            // Zeroth cleanup: create a `job_results` entry in the database.
            // TODO: how should this handle the timeout case?

            loop {
                // TODO; better control flow
                if needs_cancel {
                    break;
                }
                // A small note: tx.as_mut() will yield an executor, but &mut tx will not.
                // If I recall, this may change in a later version of sqlx.
                let mut tx = match this.db.begin().await {
                    Ok(tx) => tx,
                    Err(e) => {
                        tracing::error!("Failed to begin transaction to finalize job results: {e}");
                        break;
                    }
                };
                if let Err(e) = sqlx::query!(
                    r#"update jobs set known_state = 'not_queued' where job_id = $1;"#,
                    job_id,
                )
                .execute(tx.as_mut())
                .await
                {
                    tracing::error!("Failed to add UPDATE statement to transaction: {e}");
                    break;
                };
                let exit_status = ExitStatus::JobCanceled;
                let host_output: serde_json::Value = match serde_json::from_str("") {
                    Ok(ho) => ho,
                    Err(e) => {
                        tracing::error!("Failed to deserialize host output: {e}");
                        break;
                    }
                };
                if let Err(e) = sqlx::query!(
                    r#"insert into job_results (job_id, supervisor_id, exit_status, host_output, terminated_at) values ($1, $2, $3, $4, current_timestamp);"#,
                        job_id,
                        supervisor_id,
                        exit_status as ExitStatus,
                        host_output,
                    )
                    .execute(tx.as_mut())
                    .await
                {
                    tracing::error!("Failed to add INSERT statement to transaction: {e}.");
                    break
                };
                if let Err(e) = tx.commit().await {
                    tracing::error!("Failed to commit job result to database: {e}");
                };

                break;
            }

            // First cleanup: if the termination was caused by timeout, then we need to do something
            // about that! Specifically, we need to cancel the job. It is either the case that the
            // cancellation message is transmitted, and this works, or it does not, in which case
            // it *should* be the case that the supervisor is disconnected. In the latter case,
            // wait for the supervisor to reconnect. At such a time as it does so, then the
            // switchboard will tell it to drop the job.
            if needs_cancel {
                // if this errors, there's not much we can do.
                if let Err(e) = this.cancel(job_id, supervisor_id, true).await {
                    tracing::error!("Failed to cancel job: {e}");
                }
            }

            // TODO: is this redundant, since termination_future already finished?
            // Make sure that the supervisor is done with everything, to uphold the invariant that
            // (supervisor \in dormant set)
            //      -> (supervisor has no current job)
            //      -> (supervisor can be given work)
            {
                let job_actor = this
                    .kanban
                    .lock_metadata(job_id)
                    .await
                    .map_err(SchedError::Kanban)?;
                job_actor.wait_for_supervisor_idle().await;
            }

            // Second cleanup: we want to be able to run another job on the supervisor after this.
            // Therefore, we need a way to drop the SupervisorActorProxy (since that owns a mutex
            // guard), and unreserve the supervisor. This should, in theory, do both:
            // Unregistering should drop the job, and in turn drop the SupervisorActorProxy.
            this.kanban.unregister(job_id).map_err(SchedError::Kanban)?;

            // Third cleanup: if the job terminated due to an error in the treadmill infrastructure,
            // we should try to restart it, if it has any retry budget left in its restart policy.
            if needs_restart {
                if restart_policy.remaining_restart_count > 0 {
                    // Since users should be able to restart failed jobs at their own discretion, we
                    // factor this functionality out.
                    this.schedule_restart(init_spec).await;
                } else {
                    tracing::warn!("Job ({job_id}) meets restart qualifications, but has no remaining restarts left.");
                }
            }

            // No further cleanup is necessary.
            Ok::<(), SchedError>(())
        });

        Ok(())
    }

    async fn schedule_restart(self: &Arc<Self>, _job_init_spec: JobInitSpec) {
        //
        todo!()
    }

    /// This currently doesn't update `job_results`.
    async fn cancel(
        self: &Arc<Self>,
        job_id: Uuid,
        supervisor_id: Uuid,
        is_timeout: bool,
    ) -> Result<(), SchedError> {
        // make sure that the job doesn't get run again, and is immediately stopped if a network
        // failure or other issue prevents us from sending the StopJobMessage right now.
        sqlx::query!(
            "update jobs set known_state = 'not_queued' where job_id = $1;",
            job_id
        )
        .execute(&self.db)
        .await
        .map_err(SchedError::Database)?;
        let exit_status = if is_timeout {
            ExitStatus::HostTerminatedTimeout
        } else {
            ExitStatus::JobCanceled
        };
        let _ = sqlx::query!(
            "insert into job_results (job_id, supervisor_id, exit_status, host_output, terminated_at)\
            values ($1, $2, $3, $4, current_timestamp)",
            job_id,
            supervisor_id,
            exit_status as ExitStatus,
            // no host output for canceled jobs
            None as Option<sqlx::types::Json<()>>,
        ).execute(&self.db)
        .await
        .map_err(SchedError::Database)?;
        // Try to send a StopJobMessage
        if let Ok(mut job_actor) = self.kanban.lock_metadata(job_id).await {
            job_actor.cancel().await;
        } else {
            // Can't think of a circumstance where this would happen right now, but my gut says it
            // could if something panicked in the right place.
            tracing::error!("Scheduler failed to cancel job: job ({job_id}) not in Kanban");
        }
        Ok(())
    }
}
