use sqlx::PgExecutor;
use treadmill_rs::api::switchboard::SubjectRef;
use uuid::Uuid;

use crate::auth::engine::SubjectKind;

pub async fn subject_ref(
    conn: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<SubjectRef>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        select s.kind as "kind: SubjectKind", coalesce(u.name, g.name) as name
        from tml_switchboard.subjects s
        left join tml_switchboard.users u on u.subject_id = s.subject_id
        left join tml_switchboard.groups g on g.subject_id = s.subject_id
        where s.subject_id = $1
        "#,
        id,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| SubjectRef {
        id,
        kind: r.kind.into(),
        name: r.name,
    }))
}
