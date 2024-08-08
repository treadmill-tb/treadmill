use crate::server::auth::SubjectDetail;
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::postgres::types::PgInterval;
use sqlx::PgExecutor;
use treadmill_rs::api::switchboard::JobRequest;
use treadmill_rs::api::switchboard_supervisor::{JobInitSpec, RendezvousServerSpec, RestartPolicy};
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "restart_policy")]
struct SqlRestartPolicy {
    remaining_restart_count: i32,
}
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "rendezvous_server_spec")]
struct SqlRendezvousServerSpec {
    client_id: Uuid,
    server_base_url: String,
    auth_token: String,
}
impl From<SqlRendezvousServerSpec> for RendezvousServerSpec {
    fn from(
        SqlRendezvousServerSpec {
            client_id,
            server_base_url,
            auth_token,
        }: SqlRendezvousServerSpec,
    ) -> Self {
        Self {
            client_id,
            server_base_url,
            auth_token,
        }
    }
}
#[derive(Debug, Copy, Clone, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "job_known_state", rename_all = "snake_case")]
pub enum KnownState {
    Queued,
    Running,
    NotQueued,
}
#[derive(Debug)]
pub struct JobModel {
    job_id: Uuid,
    resume_job_id: Option<Uuid>,
    restart_job_id: Option<Uuid>,
    image_id: Option<Vec<u8>>,
    ssh_keys: Vec<String>,
    ssh_rendezvous_servers: Vec<SqlRendezvousServerSpec>,
    restart_policy: SqlRestartPolicy,
    enqueued_by_token_id: Uuid,
    tag_config: String,
    known_state: KnownState,
    timeout: PgInterval,
    #[allow(dead_code)]
    queued_at: DateTime<Utc>,
    #[allow(dead_code)]
    started_at: Option<DateTime<Utc>>,
}
impl JobModel {
    pub fn id(&self) -> Uuid {
        self.job_id
    }
    pub fn init_spec(&self) -> JobInitSpec {
        match (&self.resume_job_id, &self.restart_job_id, &self.image_id) {
            (Some(rjid), None, None) => JobInitSpec::ResumeJob { job_id: *rjid },
            (None, Some(rjid), None) => JobInitSpec::RestartJob { job_id: *rjid },
            (None, None, Some(iid)) => JobInitSpec::Image {
                image_id: ImageId(iid.clone().try_into().unwrap()),
            },
            _ => panic!("Job has resume_job_id and image_id both not null"),
        }
    }
    pub fn ssh_keys(&self) -> &[String] {
        &self.ssh_keys
    }
    pub fn rendezvous_servers(&self) -> Vec<RendezvousServerSpec> {
        self.ssh_rendezvous_servers
            .iter()
            .map(
                |SqlRendezvousServerSpec {
                     client_id,
                     server_base_url,
                     auth_token,
                 }| RendezvousServerSpec {
                    client_id: *client_id,
                    server_base_url: server_base_url.clone(),
                    auth_token: auth_token.clone(),
                },
            )
            .collect()
    }
    pub fn restart_policy(&self) -> RestartPolicy {
        RestartPolicy {
            remaining_restart_count: self.restart_policy.remaining_restart_count as usize,
        }
    }
    pub fn enqueued_by_token_id(&self) -> Uuid {
        self.enqueued_by_token_id
    }
    pub fn tag_config(&self) -> &str {
        &self.tag_config
    }
    pub fn known_state(&self) -> KnownState {
        self.known_state
    }
    pub fn timeout(&self) -> TimeDelta {
        assert_eq!(self.timeout.months, 0, "job timeouts, for practical as well as technical reasons, should not take more than 28 (*) days");
        TimeDelta::microseconds(self.timeout.microseconds)
            + TimeDelta::days(self.timeout.days as i64)
    }
}

pub async fn fetch_by_job_id(id: Uuid, db: impl PgExecutor<'_>) -> Result<JobModel, sqlx::Error> {
    sqlx::query_as!(
        JobModel,
        r#"SELECT job_id, resume_job_id, restart_job_id, image_id, ssh_keys,
                      ssh_rendezvous_servers as "ssh_rendezvous_servers: _",
                      restart_policy as "restart_policy: _", enqueued_by_token_id,
                      tag_config, known_state as "known_state: _", timeout, queued_at, started_at
               FROM jobs
               WHERE job_id = $1
               LIMIT 1;"#,
        id
    )
    .fetch_one(db)
    .await
}
pub async fn insert(
    job_id: Uuid,
    jr: &JobRequest,
    default_timeout: TimeDelta,
    subject: &SubjectDetail,
    db: impl PgExecutor<'_>,
) -> Result<JobModel, sqlx::Error> {
    let token_id = Uuid::from(subject.token_id());
    let (resume_job_id, restart_job_id, image_id) = match jr.init_spec {
        JobInitSpec::ResumeJob { job_id } => (Some(job_id), None, None),
        JobInitSpec::RestartJob { job_id } => (None, Some(job_id), None),
        JobInitSpec::Image { image_id } => (None, None, Some(image_id)),
    };
    let restart_policy = SqlRestartPolicy {
        remaining_restart_count: jr
            .restart_policy
            .remaining_restart_count
            // an insane value should be met with sane behaviour
            .try_into()
            .unwrap_or(i32::MAX),
    };
    let srs: Vec<SqlRendezvousServerSpec> = jr
        .ssh_rendezvous_servers
        .clone()
        .into_iter()
        .map(
            |RendezvousServerSpec {
                 client_id,
                 server_base_url,
                 auth_token,
             }| SqlRendezvousServerSpec {
                client_id,
                server_base_url,
                auth_token,
            },
        )
        .collect();
    let timeout = jr
        .override_timeout
        .and_then(|td| PgInterval::try_from(td).ok())
        .unwrap_or_else(|| {
            PgInterval::try_from(default_timeout)
                .expect("configured default job timeout could not be converted into PgInterval")
        });
    let now = Utc::now();
    sqlx::query!(
        r#"INSERT INTO jobs
        (job_id, resume_job_id, restart_job_id, image_id,
         ssh_keys, ssh_rendezvous_servers, restart_policy, enqueued_by_token_id, tag_config,
         known_state, timeout, queued_at, started_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, null);"#,
        &job_id,
        resume_job_id,
        restart_job_id,
        image_id.as_ref().map(ImageId::as_bytes),
        &jr.ssh_keys,
        srs.as_slice() as &[SqlRendezvousServerSpec],
        restart_policy.clone() as SqlRestartPolicy,
        token_id,
        &jr.tag_config,
        KnownState::Queued as KnownState,
        &timeout,
        now.clone(),
    )
    .execute(db)
    .await?;
    Ok(JobModel {
        job_id,
        resume_job_id,
        restart_job_id,
        image_id: image_id.map(|iid| iid.0.to_vec()),
        ssh_keys: jr.ssh_keys.clone(),
        ssh_rendezvous_servers: srs,
        restart_policy,
        enqueued_by_token_id: token_id,
        tag_config: jr.tag_config.clone(),
        known_state: KnownState::Queued,
        timeout,
        queued_at: now,
        started_at: None,
    })
}

pub mod params {
    use sqlx::PgExecutor;
    use std::collections::HashMap;
    use treadmill_rs::api::switchboard_supervisor::ParameterValue;
    use uuid::Uuid;

    #[derive(Debug, sqlx::Type)]
    #[sqlx(type_name = "parameter_value")]
    struct SqlParamValue {
        value: String,
        secret: bool,
    }
    pub async fn fetch_by_job_id(
        id: Uuid,
        db: impl PgExecutor<'_>,
    ) -> Result<HashMap<String, ParameterValue>, sqlx::Error> {
        #[derive(Debug)]
        struct SqlJobParameter {
            key: String,
            value: SqlParamValue,
        }
        sqlx::query_as!(
            SqlJobParameter,
            r#"SELECT key, value as "value: _" FROM job_parameters WHERE job_id = $1;"#,
            id
        )
        .fetch_all(db)
        .await
        .map(|v| {
            HashMap::from_iter(v.into_iter().map(
                |SqlJobParameter {
                     key,
                     value: SqlParamValue { value, secret },
                 }| (key, ParameterValue { value, secret }),
            ))
        })
    }
    pub async fn insert(
        job_id: Uuid,
        items: HashMap<String, ParameterValue>,
        db: impl PgExecutor<'_>,
    ) -> Result<(), sqlx::Error> {
        // https://github.com/launchbadge/sqlx/blob/main/FAQ.md#how-can-i-bind-an-array-to-a-values-clause-how-can-i-do-bulk-inserts
        let (keys, values): (Vec<String>, Vec<ParameterValue>) = items.into_iter().unzip();
        tracing::info!("keys={keys:?}; values={values:?}");
        let values: Vec<SqlParamValue> = values
            .into_iter()
            .map(|ParameterValue { value, secret }| SqlParamValue { value, secret })
            .collect();

        // since we have a uniform variable, we individually unnest the keys and values arrays
        sqlx::query!(
            r#"INSERT INTO job_parameters (job_id, key, value)
                SELECT $1, (c_rec).unnest, row((c_rec).value, (c_rec).secret)::parameter_value
                FROM UNNEST($2::text[], $3::parameter_value[]) as c_rec;
            "#,
            job_id,
            keys.as_slice(),
            values.as_slice() as &[SqlParamValue]
        )
        .execute(db)
        .await
        .map(|_| ())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "exit_status", rename_all = "snake_case")]
pub(crate) enum SqlExitStatus {
    FailedToMatch,
    QueueTimeout,
    HostStartFailure,
    HostTerminatedWithError,
    HostTerminatedWithSuccess,
    HostTerminatedTimeout,
    JobCanceled,
}
impl From<SqlExitStatus> for treadmill_rs::api::switchboard::ExitStatus {
    fn from(value: SqlExitStatus) -> Self {
        match value {
            SqlExitStatus::FailedToMatch => Self::FailedToMatch,
            SqlExitStatus::QueueTimeout => Self::QueueTimeout,
            SqlExitStatus::HostStartFailure => Self::HostStartFailure,
            SqlExitStatus::HostTerminatedWithError => Self::HostTerminatedWithError,
            SqlExitStatus::HostTerminatedWithSuccess => Self::HostTerminatedWithSuccess,
            SqlExitStatus::HostTerminatedTimeout => Self::HostTerminatedTimeout,
            SqlExitStatus::JobCanceled => Self::JobCanceled,
        }
    }
}
