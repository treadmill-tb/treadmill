use sqlx::PgExecutor;
use std::collections::BTreeSet;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::SqlSshEndpoint;
use crate::auth::token::SecurityToken;

pub async fn try_authenticate_supervisor(
    supervisor_id: Uuid,
    auth_token: SecurityToken,
    conn: impl PgExecutor<'_>,
) -> Result<bool, sqlx::Error> {
    let maybe_record = sqlx::query!(
        r#"select auth_token
           from tml_switchboard.supervisors where supervisor_id = $1 limit 1;"#,
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

#[derive(Debug)]
pub struct SqlSupervisor {
    pub supervisor_id: Uuid,
    pub name: String,
    pub tags: Vec<String>,
    pub ssh_endpoints: Vec<SqlSshEndpoint>,
}

pub async fn fetch_all_supervisors(
    conn: impl PgExecutor<'_>,
) -> Result<Vec<SqlSupervisor>, sqlx::Error> {
    sqlx::query_as!(
        SqlSupervisor,
        r#"select supervisor_id, name, tags, ssh_endpoints as "ssh_endpoints: _" from tml_switchboard.supervisors"#
    )
    .fetch_all(conn)
    .await
}

pub async fn insert(
    supervisor_id: Uuid,
    name: String,
    auth_token: SecurityToken,
    tag_set: &BTreeSet<String>,
    ssh_endpoints: Vec<SqlSshEndpoint>,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    let tag_vec: Vec<String> = tag_set.iter().cloned().collect();
    sqlx::query!(
        r#"
        insert into tml_switchboard.supervisors
        (supervisor_id, name, auth_token, tags, ssh_endpoints)
        values
        ($1, $2, $3, $4, $5);
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
