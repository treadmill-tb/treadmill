use chrono::Utc;
use sqlx::PgExecutor;
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
