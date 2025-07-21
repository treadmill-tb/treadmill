use sqlx::PgExecutor;
use std::collections::BTreeSet;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::SqlSshEndpoint;
// use super::job::JobId;
use crate::auth::token::SecurityToken;

super::sql_uuid_type!(SupervisorId, NullableSupervisorId);

#[derive(Debug)]
pub struct SqlSupervisor {
    pub supervisor_id: SupervisorId,
    pub name: String,
    pub tags: Vec<String>,
    pub ssh_endpoints: Vec<SqlSshEndpoint>,
    pub current_job: Option<Uuid>,
    pub worker_instance_id: i64,
}

pub async fn get(
    supervisor_id: SupervisorId,
    conn: impl PgExecutor<'_>,
) -> Result<SqlSupervisor, sqlx::Error> {
    sqlx::query_as!(
        SqlSupervisor,
        r#"
        SELECT
            supervisor_id as "supervisor_id?: SupervisorId",
            name,
            tags,
            ssh_endpoints as "ssh_endpoints: _",
            current_job,
            worker_instance_id
        FROM
            tml_switchboard.supervisors
        WHERE
            supervisor_id = $1
        "#,
        supervisor_id as SupervisorId,
    )
    .fetch_one(conn)
    .await
}

pub async fn get_all(conn: impl PgExecutor<'_>) -> Result<Vec<SqlSupervisor>, sqlx::Error> {
    sqlx::query_as!(
        SqlSupervisor,
        r#"
        SELECT
            supervisor_id as "supervisor_id!: SupervisorId",
            name,
            tags,
            ssh_endpoints as "ssh_endpoints: _",
            current_job,
            worker_instance_id
        FROM
            tml_switchboard.supervisors
        "#
    )
    .fetch_all(conn)
    .await
}

pub async fn insert(
    supervisor_id: SupervisorId,
    name: String,
    auth_token: SecurityToken,
    tag_set: &BTreeSet<String>,
    ssh_endpoints: Vec<SqlSshEndpoint>,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    let tag_vec: Vec<String> = tag_set.iter().cloned().collect();

    sqlx::query!(
        r#"
        INSERT INTO
            tml_switchboard.supervisors
        (
            supervisor_id,
            name,
            auth_token,
            tags,
            ssh_endpoints
        )
        VALUES
        (
            $1,
            $2,
            $3,
            $4,
            $5
        );
        "#,
        supervisor_id,
        name,
        auth_token.as_bytes(),
        tag_vec.as_slice(),
        ssh_endpoints.as_slice() as &[SqlSshEndpoint],
    )
    .execute(conn)
    .await
    .map(|_| ())
}

pub async fn set_current_job(
    supervisor_id: SupervisorId,
    current_job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE
            tml_switchboard.supervisors
        SET
            current_job = $1
        WHERE
            supervisor_id = $2;
        "#,
	current_job_id,
        supervisor_id,
    )
    .execute(conn)
    .await
    .map(|_| ())
}


pub async fn try_authenticate_supervisor(
    supervisor_id: SupervisorId,
    auth_token: SecurityToken,
    conn: impl PgExecutor<'_>,
) -> Result<bool, sqlx::Error> {
    let maybe_record = sqlx::query!(
        r#"
        SELECT
            auth_token
        FROM
            tml_switchboard.supervisors
        WHERE
            supervisor_id = $1
        LIMIT 1;
        "#,
        supervisor_id,
    )
    .fetch_optional(conn)
    .await?;

    let (flag, token_vec) = match maybe_record {
        Some(token_vec) => (subtle::Choice::from(1), token_vec.auth_token),
        None => (subtle::Choice::from(0), vec![0u8; 128]),
    };

    let sec_token =
        SecurityToken::try_from(token_vec).expect("stored auth token in database is invalid");

    let result = bool::from(sec_token.ct_eq(&auth_token) & ({ flag }));

    Ok(result)
}

pub async fn increment_worker_instance_id(
    supervisor_id: SupervisorId,
    conn: impl PgExecutor<'_>,
) -> Result<i64, sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE
            tml_switchboard.supervisors
        SET
            worker_instance_id = worker_instance_id + 1
        WHERE
            supervisor_id = $1
        RETURNING
            worker_instance_id
        "#,
        supervisor_id,
    )
    .fetch_one(conn)
    .await
    .map(|record| record.worker_instance_id)
}
