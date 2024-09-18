use sqlx::PgExecutor;
use uuid::Uuid;

pub async fn sql_add_user_privileges(
    user_id: Uuid,
    permissions: impl AsRef<[String]>,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        insert into tml_switchboard.user_privileges
        select $1, unnest($2::text[]);
        "#,
        user_id,
        permissions.as_ref(),
    )
    .execute(conn)
    .await?;

    Ok(())
}
