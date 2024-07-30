use crate::server::token::SecurityToken;
use chrono::Utc;
use sqlx::PgExecutor;
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Debug)]
pub struct SupervisorModel {
    supervisor_id: Uuid,
    name: String,
    last_connected_at: chrono::DateTime<Utc>,
    tags: Vec<String>,
}
impl SupervisorModel {
    pub fn id(&self) -> Uuid {
        self.supervisor_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn last_connected_at(&self) -> chrono::DateTime<Utc> {
        self.last_connected_at
    }
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
}

pub async fn fetch_by_id(
    id: Uuid,
    db: impl PgExecutor<'_>,
) -> Result<SupervisorModel, sqlx::Error> {
    sqlx::query_as!(
        SupervisorModel,
        r#"select supervisor_id, name, last_connected_at, tags from supervisors where supervisor_id = $1 limit 1;"#,
        id,
    )
    .fetch_one(db)
    .await
}

pub async fn try_authenticate(
    supervisor_id: Uuid,
    auth_token: SecurityToken,
    db: impl PgExecutor<'_>,
) -> Result<bool, sqlx::Error> {
    // how to do this without leaking timing?
    let q = sqlx::query!(
        r#"select supervisor_id, auth_token from supervisors where supervisor_id = $1 limit 1;"#,
        supervisor_id,
    )
    .fetch_one(db)
    .await;
    let v = q
        .as_ref()
        .map(|r| r.auth_token.clone())
        .unwrap_or(vec![0u8; 128]);
    let st = SecurityToken::try_from(v).expect("stored auth token in database is invalid");
    Ok(bool::from(
        st.ct_eq(&auth_token) & ({ q.is_ok() as u8 }.ct_eq(&1)),
    ))
}
