use chrono::{DateTime, TimeDelta, Utc};
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, Postgres, Transaction};
use treadmill_rs::api::switchboard::{ExitStatus, JobInitSpec, JobRequest};
use treadmill_rs::api::switchboard_supervisor::{ImageSpecification, RestartPolicy};
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.exit_status", rename_all = "snake_case")]
pub enum SqlExitStatus {
    FailedToMatch,
    QueueTimeout,
    HostStartFailure,
    HostTerminatedWithError,
    HostTerminatedWithSuccess,
    HostTerminatedTimeout,
    JobCanceled,
    UnregisteredSupervisor,
    HostDroppedJob,
}
impl From<SqlExitStatus> for ExitStatus {
    fn from(value: SqlExitStatus) -> Self {
        match value {
            SqlExitStatus::FailedToMatch => ExitStatus::FailedToMatch,
            SqlExitStatus::HostStartFailure => ExitStatus::HostStartFailure,
            SqlExitStatus::HostTerminatedTimeout => ExitStatus::HostTerminatedTimeout,
            SqlExitStatus::HostTerminatedWithError => ExitStatus::HostTerminatedWithError,
            SqlExitStatus::HostTerminatedWithSuccess => ExitStatus::HostTerminatedWithSuccess,
            SqlExitStatus::JobCanceled => ExitStatus::JobCanceled,
            SqlExitStatus::QueueTimeout => ExitStatus::QueueTimeout,
            SqlExitStatus::UnregisteredSupervisor => ExitStatus::UnregisteredSupervisor,
            SqlExitStatus::HostDroppedJob => ExitStatus::HostDroppedJob,
        }
    }
}
impl From<ExitStatus> for SqlExitStatus {
    fn from(value: ExitStatus) -> Self {
        match value {
            ExitStatus::FailedToMatch => SqlExitStatus::FailedToMatch,
            ExitStatus::HostStartFailure => SqlExitStatus::HostStartFailure,
            ExitStatus::HostTerminatedTimeout => SqlExitStatus::HostTerminatedTimeout,
            ExitStatus::HostTerminatedWithError => SqlExitStatus::HostTerminatedWithError,
            ExitStatus::HostTerminatedWithSuccess => SqlExitStatus::HostTerminatedWithSuccess,
            ExitStatus::JobCanceled => SqlExitStatus::JobCanceled,
            ExitStatus::QueueTimeout => SqlExitStatus::QueueTimeout,
            ExitStatus::UnregisteredSupervisor => SqlExitStatus::UnregisteredSupervisor,
            ExitStatus::HostDroppedJob => SqlExitStatus::HostDroppedJob,
        }
    }
}

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.restart_policy")]
pub struct SqlRestartPolicy {
    pub(crate) remaining_restart_count: i32,
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
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'queued', null, null)
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
#[sqlx(type_name = "tml_switchboard.simple_state", rename_all = "snake_case")]
pub enum SqlSimpleState {
    Queued,
    Running,
    Inactive,
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
    queued_at: DateTime<Utc>,
    simple_state: SqlSimpleState,
    started_at: Option<DateTime<Utc>>,
    running_on_supervisor_id: Option<Uuid>,
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
    pub fn running_on_supervisor_id(&self) -> Option<Uuid> {
        self.running_on_supervisor_id
    }
    pub fn simple_state(&self) -> SqlSimpleState {
        self.simple_state
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
        queued_at, simple_state as "simple_state: _", started_at, running_on_supervisor_id
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
        queued_at, simple_state as "simple_state: _", started_at, running_on_supervisor_id
        from tml_switchboard.jobs where simple_state = 'queued';
        "#
    )
    .fetch_all(conn)
    .await
}

pub async fn fetch_all_running(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        select job_id, resume_job_id, restart_job_id, image_id as "sql_image_id: _", ssh_keys,
        restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
        queued_at, simple_state as "simple_state: _", started_at, running_on_supervisor_id
        from tml_switchboard.jobs where simple_state = 'running';
        "#
    )
    .fetch_all(conn)
    .await
}

pub mod parameters {
    use sqlx::PgExecutor;
    use std::collections::HashMap;
    use treadmill_rs::api::switchboard_supervisor::ParameterValue;
    use uuid::Uuid;

    pub struct SqlJobParam {
        pub key: String,
        pub value: SqlJobParamValue,
    }
    #[derive(Debug, Clone, sqlx::Type)]
    #[sqlx(type_name = "tml_switchboard.parameter_value")]
    pub struct SqlJobParamValue {
        pub value: String,
        pub is_secret: bool,
    }

    pub async fn fetch_by_job_id(
        job_id: Uuid,
        conn: impl PgExecutor<'_>,
    ) -> Result<HashMap<String, ParameterValue>, sqlx::Error> {
        let records = sqlx::query_as!(SqlJobParam,
        r#"select key, value as "value:_" from tml_switchboard.job_parameters where job_id = $1;"#,
            job_id
        ).fetch_all(conn)
            .await?;
        Ok(records
            .into_iter()
            .map(|record| {
                (
                    record.key,
                    ParameterValue {
                        value: record.value.value,
                        secret: record.value.is_secret,
                    },
                )
            })
            .collect())
    }

    pub async fn insert(
        job_id: Uuid,
        parameters: HashMap<String, ParameterValue>,
        conn: impl PgExecutor<'_>,
    ) -> Result<(), sqlx::Error> {
        let (keys, values): (Vec<String>, Vec<ParameterValue>) = parameters.into_iter().unzip();
        let values: Vec<SqlJobParamValue> = values
            .into_iter()
            .map(|ParameterValue { value, secret }| SqlJobParamValue {
                value,
                is_secret: secret,
            })
            .collect();

        // since we have a uniform variable, we individually unnest the keys and values arrays
        sqlx::query!(
            r#"INSERT INTO tml_switchboard.job_parameters (job_id, key, value)
                SELECT $1, (c_rec).unnest, row((c_rec).value, (c_rec).is_secret)::tml_switchboard.parameter_value
                FROM UNNEST($2::text[], $3::tml_switchboard.parameter_value[]) as c_rec;
            "#,
            job_id,
            keys.as_slice(),
            values.as_slice() as &[SqlJobParamValue]
        )
        .execute(conn)
        .await
        .map(|_| ())
    }
}

pub mod history {
    use chrono::{DateTime, Utc};
    use sqlx::types::Json;
    use sqlx::PgExecutor;
    use treadmill_rs::api::switchboard::JobStatus;
    use uuid::Uuid;

    pub struct SqlJobHistoryEntry {
        pub job_id: Uuid,
        pub job_state: Json<JobStatus>,
        pub logged_at: DateTime<Utc>,
    }

    pub async fn fetch_by_job_id(
        job_id: Uuid,
        conn: impl PgExecutor<'_>,
    ) -> Result<Vec<SqlJobHistoryEntry>, sqlx::Error> {
        sqlx::query_as!(
            SqlJobHistoryEntry,
            r#"
            select job_id, job_state as "job_state: _", logged_at
            from tml_switchboard.job_state_history
            where job_id = $1;
            "#,
            job_id
        )
        .fetch_all(conn)
        .await
    }
    pub async fn fetch_most_recent_by_job_id(
        job_id: Uuid,
        conn: impl PgExecutor<'_>,
    ) -> Result<Option<SqlJobHistoryEntry>, sqlx::Error> {
        sqlx::query_as!(
            SqlJobHistoryEntry,
            r#"
            select job_id, job_state as "job_state: _", logged_at
            from tml_switchboard.job_state_history
            where job_id = $1
            order by logged_at desc
            limit 1;
            "#,
            job_id
        )
        .fetch_optional(conn)
        .await
    }
    pub async fn insert(
        job_id: Uuid,
        job_state: JobStatus,
        logged_at: DateTime<Utc>,
        conn: impl PgExecutor<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            insert into tml_switchboard.job_state_history
            values ($1, $2, $3)
            "#,
            job_id,
            Json(job_state) as Json<JobStatus>,
            logged_at
        )
        .execute(conn)
        .await
        .map(|_| ())
    }
}
