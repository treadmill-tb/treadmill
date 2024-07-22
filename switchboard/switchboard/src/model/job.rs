use crate::server::auth::SubjectDetail;
use sqlx::PgExecutor;
use treadmill_rs::api::switchboard_supervisor::{JobInitSpec, RestartPolicy};
use treadmill_rs::connector::StartJobMessage;
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

#[derive(Debug, sqlx::Type)]
#[sqlx(type_name = "restart_policy")]
struct SqlRestartPolicy {
    remaining_restart_count: i32,
}
#[derive(Debug)]
pub struct JobModel {
    job_id: Uuid,
    resume_job_id: Option<Uuid>,
    restart_job_id: Option<Uuid>,
    image_id: Option<Vec<u8>>,
    ssh_keys: Vec<String>,
    restart_policy: SqlRestartPolicy,
    queued: bool,
    enqueued_by_token_id: Uuid,
    tag_config: String,
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
    pub fn restart_policy(&self) -> RestartPolicy {
        RestartPolicy {
            remaining_restart_count: self.restart_policy.remaining_restart_count as usize,
        }
    }
    pub fn is_queued(&self) -> bool {
        self.queued
    }
    pub fn enqueued_by_token_id(&self) -> Uuid {
        self.enqueued_by_token_id
    }
    pub fn tag_config(&self) -> &str {
        &self.tag_config
    }
}

pub async fn fetch_by_job_id(id: Uuid, db: impl PgExecutor<'_>) -> Result<JobModel, sqlx::Error> {
    sqlx::query_as!(
        JobModel,
        r#"SELECT job_id, resume_job_id, restart_job_id, image_id, ssh_keys,
                      restart_policy as "restart_policy: _", queued, enqueued_by_token_id,
                      tag_config
               FROM jobs
               WHERE job_id = $1
               LIMIT 1;"#,
        id
    )
    .fetch_one(db)
    .await
}
pub async fn insert(
    jr: &StartJobMessage,
    subject: &SubjectDetail,
    db: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
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
    sqlx::query!(
        r#"INSERT INTO jobs
        (job_id, resume_job_id, restart_job_id, image_id,
         ssh_keys, restart_policy, queued, enqueued_by_token_id, tag_config)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9);"#,
        &jr.job_id,
        resume_job_id,
        restart_job_id,
        image_id.as_ref().map(ImageId::as_bytes),
        &jr.ssh_keys,
        restart_policy as SqlRestartPolicy,
        true,
        token_id,
        "",
    )
    .execute(db)
    .await
    .map(|_| ())
}

pub mod params {
    use sqlx::postgres::{PgHasArrayType, PgTypeInfo};
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
    impl PgHasArrayType for SqlParamValue {
        fn array_type_info() -> PgTypeInfo {
            PgTypeInfo::with_name("_parameter_value")
        }
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
        let values: Vec<SqlParamValue> = values
            .into_iter()
            .map(|ParameterValue { value, secret }| SqlParamValue { value, secret })
            .collect();

        // since we have a uniform variable, we individually unnest the keys and values arrays
        sqlx::query!(
            r#"
        INSERT INTO job_parameters (job_id, key, value)
            SELECT $1, keys, values
            FROM UNNEST($2::text[]) as keys,
                 UNNEST($3::parameter_value[]) as values
        ;
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
