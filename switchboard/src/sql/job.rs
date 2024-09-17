use chrono::{DateTime, TimeDelta, Utc};
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, Postgres, Transaction};
use treadmill_rs::api::switchboard::{ExitStatus, JobInitSpec, JobRequest};
use treadmill_rs::api::switchboard_supervisor::{ImageSpecification, RestartPolicy};
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

pub mod history;
pub mod parameters;

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.exit_status", rename_all = "snake_case")]
pub enum SqlExitStatus {
    SupervisorMatchError,
    QueueTimeout,
    InternalSupervisorError,
    SupervisorHostStartError,
    SupervisorDroppedJob,
    JobCanceled,
    JobTimeout,
    WorkloadFinishedError,
    WorkloadFinishedSuccess,
    WorkloadFinishedUnknown,
}
impl From<SqlExitStatus> for ExitStatus {
    fn from(value: SqlExitStatus) -> Self {
        match value {
            SqlExitStatus::SupervisorMatchError => ExitStatus::SupervisorMatchError,
            SqlExitStatus::QueueTimeout => ExitStatus::QueueTimeout,
            SqlExitStatus::InternalSupervisorError => ExitStatus::InternalSupervisorError,
            SqlExitStatus::SupervisorHostStartError => ExitStatus::SupervisorHostStartFailure,
            SqlExitStatus::SupervisorDroppedJob => ExitStatus::SupervisorDroppedJob,
            SqlExitStatus::JobCanceled => ExitStatus::JobCanceled,
            SqlExitStatus::JobTimeout => ExitStatus::JobTimeout,
            SqlExitStatus::WorkloadFinishedSuccess => ExitStatus::WorkloadFinishedSuccess,
            SqlExitStatus::WorkloadFinishedError => ExitStatus::WorkloadFinishedError,
            SqlExitStatus::WorkloadFinishedUnknown => ExitStatus::WorkloadFinishedUnknown,
        }
    }
}
impl From<ExitStatus> for SqlExitStatus {
    fn from(value: ExitStatus) -> Self {
        match value {
            ExitStatus::SupervisorMatchError => SqlExitStatus::SupervisorMatchError,
            ExitStatus::QueueTimeout => SqlExitStatus::QueueTimeout,
            ExitStatus::InternalSupervisorError => SqlExitStatus::InternalSupervisorError,
            ExitStatus::SupervisorHostStartFailure => SqlExitStatus::SupervisorHostStartError,
            ExitStatus::SupervisorDroppedJob => SqlExitStatus::SupervisorDroppedJob,
            ExitStatus::JobCanceled => SqlExitStatus::JobCanceled,
            ExitStatus::JobTimeout => SqlExitStatus::JobTimeout,
            ExitStatus::WorkloadFinishedSuccess => SqlExitStatus::WorkloadFinishedSuccess,
            ExitStatus::WorkloadFinishedError => SqlExitStatus::WorkloadFinishedError,
            ExitStatus::WorkloadFinishedUnknown => SqlExitStatus::WorkloadFinishedUnknown,
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
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                'queued', $10, null, null, null, null, null, default)
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

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.functional_state",
    rename_all = "snake_case"
)]
pub enum SqlFunctionalState {
    Queued,
    Dispatched,
    Finalized,
}

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

    functional_state: SqlFunctionalState,

    // Filled out when initialized into `queued` functional state
    queued_at: DateTime<Utc>,

    // Filled out if and when transitioned into `dispatched` functional state
    started_at: Option<DateTime<Utc>>,
    dispatched_on_supervisor_id: Option<Uuid>,

    // Filled out when transitioned into `finalized` functional state
    #[allow(dead_code)]
    exit_status: Option<SqlExitStatus>,
    #[allow(dead_code)]
    host_output: Option<String>,
    #[allow(dead_code)]
    terminated_at: Option<DateTime<Utc>>,

    #[allow(dead_code)]
    last_updated_at: DateTime<Utc>,
}

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
    pub fn functional_state(&self) -> SqlFunctionalState {
        self.functional_state
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
        queued_at, functional_state as "functional_state: _", started_at,
        dispatched_on_supervisor_id, exit_status as "exit_status: _", host_output, terminated_at,
        last_updated_at
        from tml_switchboard.jobs where job_id = $1;
        "#,
        job_id
    )
    .fetch_one(conn)
    .await
}

pub async fn fetch_all_queued(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
        restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
        queued_at, functional_state as "functional_state: _", started_at,
        dispatched_on_supervisor_id, exit_status as "exit_status: _", host_output, terminated_at,
        last_updated_at
        from tml_switchboard.jobs where functional_state = 'queued';
        "#
    )
    .fetch_all(conn)
    .await
}

pub async fn fetch_all_dispatched(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
        restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
        queued_at, functional_state as "functional_state: _", started_at,
        dispatched_on_supervisor_id, exit_status as "exit_status: _", host_output, terminated_at,
        last_updated_at
        from tml_switchboard.jobs where functional_state = 'dispatched';
        "#
    )
    .fetch_all(conn)
    .await
}

// #[derive(Debug, Copy, Clone, sqlx::Type)]
// #[sqlx(
//     type_name = "tml_switchboard.execution_status",
//     rename_all = "snake_case"
// )]
// pub enum SqlExecutionStatus {
//     Queued,
//     Scheduled,
//     Initializing,
//     Ready,
//     Terminating,
//     Terminated,
// }
