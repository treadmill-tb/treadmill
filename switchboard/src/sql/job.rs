use super::SqlSshEndpoint;
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, Postgres, Transaction};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, TerminationReason};
use treadmill_rs::api::switchboard_supervisor::{
    ImageSpecification, JobInitializingStage, RestartPolicy, TaskExitStatus,
};
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

pub mod parameters;

#[derive(Debug, Copy, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.job_state", rename_all = "snake_case")]
pub enum SqlJobState {
    Queued,
    Scheduled,
    Initializing,
    Ready,
    Terminating,
    Finalized,
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.job_initializing_stage",
    rename_all = "snake_case"
)]
pub enum SqlJobInitializingStage {
    Starting,
    FetchingImage,
    Allocating,
    Provisioning,
    Booting,
}
impl From<SqlJobInitializingStage> for JobInitializingStage {
    fn from(value: SqlJobInitializingStage) -> Self {
        match value {
            SqlJobInitializingStage::Starting => JobInitializingStage::Starting,
            SqlJobInitializingStage::FetchingImage => JobInitializingStage::FetchingImage,
            SqlJobInitializingStage::Allocating => JobInitializingStage::Allocating,
            SqlJobInitializingStage::Provisioning => JobInitializingStage::Provisioning,
            SqlJobInitializingStage::Booting => JobInitializingStage::Booting,
        }
    }
}
impl From<JobInitializingStage> for SqlJobInitializingStage {
    fn from(value: JobInitializingStage) -> Self {
        match value {
            JobInitializingStage::Starting => SqlJobInitializingStage::Starting,
            JobInitializingStage::FetchingImage => SqlJobInitializingStage::FetchingImage,
            JobInitializingStage::Allocating => SqlJobInitializingStage::Allocating,
            JobInitializingStage::Provisioning => SqlJobInitializingStage::Provisioning,
            JobInitializingStage::Booting => SqlJobInitializingStage::Booting,
        }
    }
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.termination_reason",
    rename_all = "snake_case"
)]
pub enum SqlTerminationReason {
    WorkloadExited,
    WorkloadSelfCanceled,
    UserCanceled,
    QueueTimeout,
    ExecutionTimeout,
    ImageError,
    SupervisorMatchError,
    SupervisorHostStartFailure,
    SupervisorDroppedJob,
    SupervisorUnreachable,
    ResumeFailed,
    InternalError,
}
impl From<SqlTerminationReason> for TerminationReason {
    fn from(value: SqlTerminationReason) -> Self {
        match value {
            SqlTerminationReason::WorkloadExited => TerminationReason::WorkloadExited,
            SqlTerminationReason::WorkloadSelfCanceled => TerminationReason::WorkloadSelfCanceled,
            SqlTerminationReason::UserCanceled => TerminationReason::UserCanceled,
            SqlTerminationReason::QueueTimeout => TerminationReason::QueueTimeout,
            SqlTerminationReason::ExecutionTimeout => TerminationReason::ExecutionTimeout,
            SqlTerminationReason::ImageError => TerminationReason::ImageError,
            SqlTerminationReason::SupervisorMatchError => TerminationReason::SupervisorMatchError,
            SqlTerminationReason::SupervisorHostStartFailure => {
                TerminationReason::SupervisorHostStartFailure
            }
            SqlTerminationReason::SupervisorDroppedJob => TerminationReason::SupervisorDroppedJob,
            SqlTerminationReason::SupervisorUnreachable => TerminationReason::SupervisorUnreachable,
            SqlTerminationReason::ResumeFailed => TerminationReason::ResumeFailed,
            SqlTerminationReason::InternalError => TerminationReason::InternalError,
        }
    }
}
impl From<TerminationReason> for SqlTerminationReason {
    fn from(value: TerminationReason) -> Self {
        match value {
            TerminationReason::WorkloadExited => SqlTerminationReason::WorkloadExited,
            TerminationReason::WorkloadSelfCanceled => SqlTerminationReason::WorkloadSelfCanceled,
            TerminationReason::UserCanceled => SqlTerminationReason::UserCanceled,
            TerminationReason::QueueTimeout => SqlTerminationReason::QueueTimeout,
            TerminationReason::ExecutionTimeout => SqlTerminationReason::ExecutionTimeout,
            TerminationReason::ImageError => SqlTerminationReason::ImageError,
            TerminationReason::SupervisorMatchError => SqlTerminationReason::SupervisorMatchError,
            TerminationReason::SupervisorHostStartFailure => {
                SqlTerminationReason::SupervisorHostStartFailure
            }
            TerminationReason::SupervisorDroppedJob => SqlTerminationReason::SupervisorDroppedJob,
            TerminationReason::SupervisorUnreachable => SqlTerminationReason::SupervisorUnreachable,
            TerminationReason::ResumeFailed => SqlTerminationReason::ResumeFailed,
            TerminationReason::InternalError => SqlTerminationReason::InternalError,
        }
    }
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.task_exit_status",
    rename_all = "snake_case"
)]
pub enum SqlTaskExitStatus {
    Pending,
    Success,
    Failure,
}
impl From<SqlTaskExitStatus> for TaskExitStatus {
    fn from(value: SqlTaskExitStatus) -> Self {
        match value {
            SqlTaskExitStatus::Pending => TaskExitStatus::Pending,
            SqlTaskExitStatus::Success => TaskExitStatus::Success,
            SqlTaskExitStatus::Failure => TaskExitStatus::Failure,
        }
    }
}
impl From<TaskExitStatus> for SqlTaskExitStatus {
    fn from(value: TaskExitStatus) -> Self {
        match value {
            TaskExitStatus::Pending => SqlTaskExitStatus::Pending,
            TaskExitStatus::Success => SqlTaskExitStatus::Success,
            TaskExitStatus::Failure => SqlTaskExitStatus::Failure,
        }
    }
}

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.restart_policy")]
pub struct SqlRestartPolicy {
    pub remaining_restart_count: i32,
}
impl From<SqlRestartPolicy> for RestartPolicy {
    fn from(value: SqlRestartPolicy) -> Self {
        Self {
            remaining_restart_count: usize::try_from(value.remaining_restart_count).unwrap_or(0),
        }
    }
}

pub async fn insert(
    job_request: JobRequest,
    as_job_id: Uuid,
    as_token_id: Uuid,
    job_timeout: PgInterval,
    queued_at: DateTime<Utc>,
    conn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    let (resume_job_id, restart_job_id, image_id): (Option<Uuid>, Option<Uuid>, Option<Vec<u8>>) =
        match job_request.init_spec {
            JobInitSpec::ResumeJob { job_id } => (Some(job_id), None, None),
            JobInitSpec::RestartJob { job_id } => {
                let predecessor = sqlx::query!(
                    r#"
                    select resume_job_id, restart_job_id, image_id
                    from tml_switchboard.jobs
                    where job_id = $1
                    "#,
                    job_id
                )
                .fetch_one(conn.as_mut())
                .await?;
                (
                    predecessor.resume_job_id,
                    Some(job_id),
                    predecessor.image_id,
                )
            }
            JobInitSpec::Image { image_id } => (None, None, Some(image_id.0.to_vec())),
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
          job_state,
          initializing_stage,
          queued_at,
          started_at,
          dispatched_on_supervisor_id,
          ssh_endpoints,
          termination_reason,
          task_exit_status,
          exit_message,
          terminated_at,
          last_updated_at
        )
        values (
          $1,       -- job_id
          $2,	    -- resume_job_id
          $3,	    -- restart_job_id
          $4,	    -- image_id
          $5,	    -- ssh_keys
          $6,	    -- restart_policy
          $7,	    -- enqueued_by_token_id
          $8,	    -- tag_config
          $9,	    -- job_timeout
          'queued', -- job_state
          null,	    -- initializing_stage
          $10,	    -- queued_at
          null,	    -- started_at
          null,	    -- dispatched_on_supervisor_id
          null,	    -- ssh_endpoints
          null,	    -- termination_reason
          null,	    -- task_exit_status
          null,	    -- exit_message
          null,	    -- terminated_at
          default   -- last_updated_at
        )
        "#,
        as_job_id,
        resume_job_id,
        restart_job_id,
        image_id,
        job_request.ssh_keys.as_slice(),
        SqlRestartPolicy {
            remaining_restart_count: i32::try_from(
                job_request.restart_policy.remaining_restart_count
            )
            .unwrap(),
        } as SqlRestartPolicy,
        as_token_id,
        job_request.tag_config,
        job_timeout,
        queued_at,
    )
    .execute(conn.as_mut())
    .await?;

    Ok(())
}

#[allow(dead_code)]
pub struct SqlJob {
    job_id: Uuid,
    resume_job_id: Option<Uuid>,
    #[allow(dead_code)]
    restart_job_id: Option<Uuid>,
    sql_image_id: Option<Vec<u8>>,

    ssh_keys: Vec<String>,
    sql_restart_policy: SqlRestartPolicy,
    enqueued_by_token_id: Uuid,
    tag_config: String,
    job_timeout: PgInterval,

    job_state: SqlJobState,

    // Filled out while `job_state = 'initializing'`; null otherwise.
    #[allow(dead_code)]
    initializing_stage: Option<SqlJobInitializingStage>,

    // Filled out when initialized into `queued` job state
    queued_at: DateTime<Utc>,

    // Filled out if and when the job is dispatched onto a supervisor
    started_at: Option<DateTime<Utc>>,
    dispatched_on_supervisor_id: Option<Uuid>,
    ssh_endpoints: Option<Vec<SqlSshEndpoint>>,

    // Filled out when transitioned into `finalized` job state
    #[allow(dead_code)]
    termination_reason: Option<SqlTerminationReason>,
    #[allow(dead_code)]
    task_exit_status: Option<SqlTaskExitStatus>,
    #[allow(dead_code)]
    exit_message: Option<String>,
    #[allow(dead_code)]
    terminated_at: Option<DateTime<Utc>>,

    #[allow(dead_code)]
    last_updated_at: DateTime<Utc>,
}

#[allow(dead_code)]
impl SqlJob {
    pub fn job_id(&self) -> Uuid {
        self.job_id
    }
    pub fn read_image_spec(&self) -> ImageSpecification {
        if let Some(resume_job_id) = self.resume_job_id {
            ImageSpecification::ResumeJob {
                job_id: resume_job_id,
            }
        } else {
            ImageSpecification::Image {
                image_id: self
                    .sql_image_id
                    .as_ref()
                    .expect("image_id column cannot be NULL if resume_job_id column is NULL")
                    .as_slice()
                    .try_into()
                    .map(ImageId)
                    .expect("image_id in database has wrong length"),
            }
        }
    }
    pub fn ssh_keys(&self) -> &[String] {
        &self.ssh_keys
    }
    pub fn restart_policy(&self) -> RestartPolicy {
        self.sql_restart_policy.clone().into()
    }
    pub fn enqueued_by_token_id(&self) -> Uuid {
        self.enqueued_by_token_id
    }
    pub fn raw_tag_config(&self) -> &str {
        &self.tag_config
    }
    pub fn timeout(&self) -> TimeDelta {
        assert_eq!(
            self.job_timeout.months, 0,
            "invariant violation: job_timeout.months SHALL BE 0"
        );
        TimeDelta::microseconds(self.job_timeout.microseconds)
            + TimeDelta::days(i64::from(self.job_timeout.days))
    }
    pub fn queued_at(&self) -> &DateTime<Utc> {
        &self.queued_at
    }
    pub fn started_at(&self) -> Option<&DateTime<Utc>> {
        self.started_at.as_ref()
    }
    pub fn dispatched_on_supervisor_id(&self) -> Option<Uuid> {
        self.dispatched_on_supervisor_id
    }
    pub fn ssh_endpoints(&self) -> Option<&Vec<SqlSshEndpoint>> {
        self.ssh_endpoints.as_ref()
    }
    pub fn job_state(&self) -> SqlJobState {
        self.job_state
    }
}

pub async fn fetch_by_job_id(
    job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<SqlJob, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
        restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
        queued_at, job_state as "job_state: _",
        initializing_stage as "initializing_stage: _", started_at,
        dispatched_on_supervisor_id, ssh_endpoints as "ssh_endpoints: _",
        termination_reason as "termination_reason: _",
        task_exit_status as "task_exit_status: _", exit_message, terminated_at,
        last_updated_at
        from tml_switchboard.jobs where job_id = $1;
        "#,
        job_id
    )
    .fetch_one(conn)
    .await
}

// pub async fn fetch_all_queued(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
//     sqlx::query_as!(
//         SqlJob,
//         r#"
//         select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
//         restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
//         queued_at, job_state as "job_state: _",
//         initializing_stage as "initializing_stage: _", started_at,
//         dispatched_on_supervisor_id, ssh_endpoints as "ssh_endpoints: _",
//         termination_reason as "termination_reason: _",
//         task_exit_status as "task_exit_status: _", exit_message, terminated_at,
//         last_updated_at
//         from tml_switchboard.jobs where job_state = 'queued';
//         "#
//     )
//     .fetch_all(conn)
//     .await
// }

// pub async fn fetch_all_dispatched(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
//     sqlx::query_as!(
//         SqlJob,
//         r#"
//         select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
//         restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
//         queued_at, job_state as "job_state: _",
//         initializing_stage as "initializing_stage: _", started_at,
//         dispatched_on_supervisor_id, ssh_endpoints as "ssh_endpoints: _",
//         termination_reason as "termination_reason: _",
//         task_exit_status as "task_exit_status: _", exit_message, terminated_at,
//         last_updated_at
//         from tml_switchboard.jobs
//         where job_state in ('scheduled', 'initializing', 'ready', 'terminating');
//         "#
//     )
//     .fetch_all(conn)
//     .await
// }

/// Finalize a job that its supervisor dropped, releasing the supervisor and --
/// if the restart policy permits -- enqueuing a successor, all in one
/// transaction. Backs reconciliation cases 3 and 5 (`Phase 5`): the switchboard
/// believed `job_id` was assigned to `supervisor_id`, but the supervisor no
/// longer reports running it.
///
/// The job is finalized with [`TerminationReason::SupervisorDroppedJob`], its
/// assignment columns (`dispatched_on_supervisor_id`, `started_at`,
/// `initializing_stage`) are cleared to satisfy the finalized-state invariants,
/// a `FinalizeResult` event is appended to the audit log, and
/// `supervisors.current_job` is released (guarded on `job_id` so a pointer that
/// has since been reassigned is left alone).
///
/// **Idempotent.** The finalize only fires when the job is not already
/// finalized; a replayed reconciliation therefore neither double-finalizes nor
/// spawns a second restart successor. Returns the successor's job id if one was
/// enqueued.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness
/// guard covers it.
/// Adopt a supervisor-reported *executing* state into the DB `job_state`, used
/// by reconciliation case 4 (the supervisor reports `OngoingJob(J_sb)` for the
/// assigned job, so the switchboard takes the reported state as ground truth).
///
/// `job_state` must be one of the executing states (`initializing`, `ready`,
/// `terminating`); `initializing_stage` is required exactly when `job_state` is
/// `initializing` and must be `None` otherwise. The terminal `finalized` state
/// is deliberately *not* expressible here: a supervisor-reported `Terminated`
/// needs a [`TerminationReason`] and so goes through a finalize path, not this
/// one (see the `RunningJobState` Rustdoc on the `Terminated` fold).
///
/// `started_at` is back-filled to `at` if it was null (e.g. adopting `ready`
/// over a still-`scheduled` row), satisfying the `started_at_iso_executing`
/// CHECK. Idempotent: re-adopting the same state is a harmless rewrite.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness
/// guard covers it.
pub async fn set_running_state(
    job_id: Uuid,
    job_state: SqlJobState,
    initializing_stage: Option<SqlJobInitializingStage>,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = $2,
            initializing_stage = $3,
            started_at = coalesce(started_at, $4),
            last_updated_at = default
        where job_id = $1
        "#,
        job_id,
        job_state as SqlJobState,
        initializing_stage as Option<SqlJobInitializingStage>,
        at,
    )
    .execute(&mut **txn)
    .await?;
    Ok(())
}

/// Finalize a job the supervisor reports as `Terminated`, within the caller's
/// transaction. Backs reconciliation case 4 when the adopted `RunningJobState`
/// is `Terminated`: the supervisor's workload exited, so the job finalizes with
/// [`TerminationReason::WorkloadExited`].
///
/// The task outcome (`task_exit_status` / `exit_message`) is *not* set here — it
/// is reported out-of-band via [`set_task_outcome`], so whatever the supervisor
/// last declared is preserved across this transition. The assignment columns
/// (`dispatched_on_supervisor_id`, `started_at`, `initializing_stage`) are
/// cleared to satisfy the finalized-state invariants and
/// `supervisors.current_job` is released (guarded on `job_id`). Unlike
/// [`finalize_dropped_and_maybe_restart`], the restart policy is **not** applied:
/// a clean workload exit is a completion, not a failure to retry.
///
/// **Idempotent.** The finalize only fires when the job is not already
/// finalized, so a replayed reconciliation is a no-op.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness
/// guard covers it.
pub async fn finalize_terminated(
    job_id: Uuid,
    supervisor_id: Uuid,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    let transitioned = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = 'finalized',
            termination_reason = 'workload_exited',
            dispatched_on_supervisor_id = null,
            started_at = null,
            initializing_stage = null,
            terminated_at = $2,
            last_updated_at = default
        where job_id = $1 and job_state <> 'finalized'
        returning job_id
        "#,
        job_id,
        at,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // Some earlier pass already finalized this job: nothing left to do.
    if transitioned.is_none() {
        return Ok(());
    }

    // Release the supervisor's assignment pointer.
    sqlx::query!(
        r#"
        update tml_switchboard.supervisors
        set current_job = null
        where supervisor_id = $1 and current_job = $2
        "#,
        supervisor_id,
        job_id,
    )
    .execute(&mut **txn)
    .await?;

    Ok(())
}

/// Record the supervisor's *task outcome* for a job it is currently assigned,
/// within the caller's transaction.
///
/// This is the dedicated setter behind [`SupervisorJobEvent::DeclareExitStatus`].
/// It is independent of the job's lifecycle state: the supervisor may set the
/// outcome at any point while the job is dispatched to it, as many times as it
/// likes, each call overriding the previous `task_exit_status` and replacing
/// `exit_message` (passing `None` clears the message). The outcome itself
/// (`pending` / `success` / `failure`) can never be cleared back to unset.
///
/// The write is guarded on `dispatched_on_supervisor_id = supervisor_id`, which
/// is precisely "the job is assigned to this supervisor": it is a no-op (returns
/// `false`) for a job assigned to someone else, never dispatched, or already
/// finalized (whose dispatch pointer has been cleared).
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness guard
/// covers it.
pub async fn set_task_outcome(
    job_id: Uuid,
    supervisor_id: Uuid,
    outcome: SqlTaskExitStatus,
    message: Option<String>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set task_exit_status = $3,
            exit_message = $4,
            last_updated_at = default
        where job_id = $1 and dispatched_on_supervisor_id = $2
        returning job_id
        "#,
        job_id,
        supervisor_id,
        outcome as SqlTaskExitStatus,
        message,
    )
    .fetch_optional(&mut **txn)
    .await?;

    Ok(updated.is_some())
}

pub async fn finalize_dropped_and_maybe_restart(
    job_id: Uuid,
    supervisor_id: Uuid,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<Option<Uuid>, sqlx::Error> {
    // Read the predecessor's restart-relevant fields before finalizing.
    let predecessor = fetch_by_job_id(job_id, &mut **txn).await?;

    // Finalize only if not already finalized; the returned row tells us whether
    // this pass is the one that actually performed the transition.
    let transitioned = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = 'finalized',
            termination_reason = 'supervisor_dropped_job',
            task_exit_status = null,
            dispatched_on_supervisor_id = null,
            started_at = null,
            initializing_stage = null,
            terminated_at = $2,
            last_updated_at = default
        where job_id = $1 and job_state <> 'finalized'
        returning job_id
        "#,
        job_id,
        at,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // Some earlier pass already finalized this job: nothing left to do.
    if transitioned.is_none() {
        return Ok(None);
    }

    // Release the supervisor's assignment pointer.
    sqlx::query!(
        r#"
        update tml_switchboard.supervisors
        set current_job = null
        where supervisor_id = $1 and current_job = $2
        "#,
        supervisor_id,
        job_id,
    )
    .execute(&mut **txn)
    .await?;

    // Honor the restart policy: enqueue a successor with one fewer restart.
    let remaining = predecessor.sql_restart_policy.remaining_restart_count;
    if remaining <= 0 {
        return Ok(None);
    }

    let successor_id = Uuid::new_v4();
    let parameters = parameters::fetch_by_job_id(job_id, &mut **txn).await?;
    let job_request = JobRequest {
        init_spec: JobInitSpec::RestartJob { job_id },
        ssh_keys: predecessor.ssh_keys.clone(),
        restart_policy: RestartPolicy {
            remaining_restart_count: usize::try_from(remaining - 1).unwrap_or(0),
        },
        parameters: parameters.clone(),
        tag_config: predecessor.tag_config.clone(),
        override_timeout: None,
    };
    insert(
        job_request,
        successor_id,
        predecessor.enqueued_by_token_id,
        predecessor.job_timeout,
        at,
        txn,
    )
    .await?;
    parameters::insert(successor_id, parameters, &mut **txn).await?;

    Ok(Some(successor_id))
}
