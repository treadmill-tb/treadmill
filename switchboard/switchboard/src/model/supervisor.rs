use chrono::Utc;
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::VerifyingKey;
use sqlx::PgExecutor;
use uuid::Uuid;

#[derive(Debug)]
pub struct SupervisorModel {
    supervisor_id: Uuid,
    name: String,
    last_connected_at: chrono::DateTime<Utc>,
    public_key: String,
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
    pub fn public_key(&self) -> ed25519_dalek::pkcs8::spki::Result<VerifyingKey> {
        VerifyingKey::from_public_key_pem(&self.public_key)
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
        r#"select * from supervisors where supervisor_id = $1 limit 1;"#,
        id,
    )
    .fetch_one(db)
    .await
}
