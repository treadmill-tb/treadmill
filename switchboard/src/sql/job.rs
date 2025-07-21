use chrono::{DateTime, TimeDelta, Utc};
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, Postgres, Transaction};
use treadmill_rs::api::switchboard::{ExitStatus, JobInitSpec, JobRequest};
use treadmill_rs::api::switchboard_supervisor::{ImageSpecification, RestartPolicy};
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

use super::SqlSshEndpoint;
use super::supervisor::SupervisorId;

pub mod history;
pub mod parameters;

// super::sql_uuid_type!(JobId, NullableJobId);

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
    job_id: Uuid,
    token_id: Uuid,
    job_timeout: PgInterval,
    enqueued_at: DateTime<Utc>,
    conn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    let (resume_job_id, restart_job_id, image_id): (Option<Uuid>, Option<Uuid>, Option<Vec<u8>>) =
        match job_request.init_spec {
            JobInitSpec::ResumeJob { job_id: init_spec_job_id } => (Some(init_spec_job_id), None, None),
            JobInitSpec::RestartJob { job_id: init_spec_job_id } => {
                let predecessor = sqlx::query!(
                    r#"
                    select resume_job_id, restart_job_id, image_id
                    from tml_switchboard.jobs
                    where job_id = $1
                    "#,
                    init_spec_job_id
                )
                .fetch_one(conn.as_mut())
                .await?;
                (
                    predecessor.resume_job_id,
                    Some(init_spec_job_id),
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
          enqueued_at,
          started_at,
          assigned_supervisor,
          ssh_endpoints,
          exit_status,
          host_output,
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
          'enqueued', -- job_state
          $10,	    -- enqueued_at
          null,	    -- started_at
          null,	    -- assigned_supervisor
          null,	    -- ssh_endpoints
          null,	    -- exit_status
          null,	    -- host_output
          null,	    -- terminated_at
          default   -- last_updated_at
        )
        "#,
        job_id,
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
        token_id,
        job_request.tag_config,
        job_timeout,
        enqueued_at,
    )
    .execute(conn.as_mut())
    .await?;

    Ok(())
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.job_state",
    rename_all = "snake_case"
)]
pub enum SqlJobState {
    Enqueued,
    Dispatching,
    Dispatched,
    Finalized,
}

#[derive(Debug)]
pub struct SqlJob {
    pub job_id: Uuid,
    pub resume_job_id: Option<Uuid>,
    #[allow(dead_code)]
    pub restart_job_id: Option<Uuid>,
    pub sql_image_id: Option<Vec<u8>>,

    pub ssh_keys: Vec<String>,
    pub sql_restart_policy: SqlRestartPolicy,
    pub enqueued_by_token_id: Uuid,
    pub tag_config: String,
    pub job_timeout: PgInterval,

    pub job_state: SqlJobState,

    // Filled out when initialized into `enqueued` functional state
    pub enqueued_at: DateTime<Utc>,

    // Filled out if and when transitioned into `dispatched` functional state
    pub started_at: Option<DateTime<Utc>>,
    pub assigned_supervisor: Option<SupervisorId>,
    pub ssh_endpoints: Option<Vec<SqlSshEndpoint>>,

    // Filled out when transitioned into `finalized` functional state
    #[allow(dead_code)]
    pub exit_status: Option<SqlExitStatus>,
    #[allow(dead_code)]
    pub host_output: Option<String>,
    #[allow(dead_code)]
    pub terminated_at: Option<DateTime<Utc>>,

    #[allow(dead_code)]
    pub last_updated_at: DateTime<Utc>,
}

impl SqlJob {
    pub fn job_id(&self) -> Uuid {
        self.job_id
    }
    pub fn read_image_spec(&self) -> ImageSpecification {
        if let Some(resume_job_id) = self.resume_job_id {
            ImageSpecification::ResumeJob {
                job_id: resume_job_id.into(),
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
    pub fn enqueued_at(&self) -> &DateTime<Utc> {
        &self.enqueued_at
    }
    pub fn started_at(&self) -> Option<&DateTime<Utc>> {
        self.started_at.as_ref()
    }
    pub fn assigned_supervisor(&self) -> Option<SupervisorId> {
        self.assigned_supervisor
    }
    pub fn ssh_endpoints(&self) -> Option<&Vec<SqlSshEndpoint>> {
        self.ssh_endpoints.as_ref()
    }
    pub fn job_state(&self) -> SqlJobState {
        self.job_state
    }
}

pub async fn get(job_id: Uuid, conn: impl PgExecutor<'_>) -> Result<SqlJob, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        SELECT
            job_id,
            resume_job_id,
            restart_job_id,
            image_id as "sql_image_id: _",
            ssh_keys,
            restart_policy as "sql_restart_policy: _",
            enqueued_by_token_id,
            tag_config,
            job_timeout,
            enqueued_at,
            job_state as "job_state: _",
            started_at,
            assigned_supervisor,
            ssh_endpoints as "ssh_endpoints: _",
            exit_status as "exit_status: _",
            host_output,
            terminated_at,
            last_updated_at
        FROM
            tml_switchboard.jobs
        WHERE
            job_id = $1;
        "#,
        job_id
    )
    .fetch_one(conn)
    .await
}

pub async fn find_next_enqueued_for_supervisor(
    supervisor_id: SupervisorId,
    conn: impl PgExecutor<'_>,
) -> Result<Option<Uuid>, sqlx::Error> {
    let res = sqlx::query!(
        r#"
        SELECT
            job_id
        FROM
            tml_switchboard.jobs
        WHERE
            job_state = 'enqueued'
        AND
            assigned_supervisor = $1
        ORDER BY
            enqueued_at
        LIMIT 1;
        "#,
        supervisor_id,
    )
    .fetch_one(conn)
    .await;

    match res {
        Ok(record) => Ok(Some(record.job_id)),
        Err(sqlx::Error::RowNotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

pub async fn update_job_state(
    job_id: Uuid,
    new_state: SqlJobState,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE
            tml_switchboard.jobs
        SET
            job_state = $1
        WHERE
            job_id = $2;
        "#,
        new_state as SqlJobState,
	job_id,
    )
	.execute(conn).await
	.map(|_| ())
}

pub async fn fetch_all_enqueued(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
        restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
        enqueued_at, job_state as "job_state: _", started_at,
        assigned_supervisor, ssh_endpoints as "ssh_endpoints: _",
        exit_status as "exit_status: _", host_output, terminated_at, last_updated_at
        from tml_switchboard.jobs where job_state = 'enqueued';
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
        enqueued_at, job_state as "job_state: _", started_at,
        assigned_supervisor, ssh_endpoints as "ssh_endpoints: _",
        exit_status as "exit_status: _", host_output, terminated_at, last_updated_at
        from tml_switchboard.jobs where job_state = 'dispatched';
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
//     Enqueued,
//     Scheduled,
//     Initializing,
//     Ready,
//     Terminating,
//     Terminated,
// }
