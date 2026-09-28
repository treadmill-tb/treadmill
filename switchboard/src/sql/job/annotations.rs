use sqlx::{PgExecutor, Postgres, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

/// A job's annotations, keyed by name, from the `job_annotations` table.
pub async fn fetch_by_job_id(
    job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<BTreeMap<String, String>, sqlx::Error> {
    let records = sqlx::query!(
        r#"select key, value from tml_switchboard.job_annotations where job_id = $1"#,
        job_id
    )
    .fetch_all(conn)
    .await?;
    Ok(records.into_iter().map(|r| (r.key, r.value)).collect())
}

/// Insert a new job's annotations.
///
/// To perform this action as part of a transaction, pass `transaction.as_mut()` as the connection
/// parameter.
pub async fn insert(
    job_id: Uuid,
    annotations: &BTreeMap<String, String>,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    let (keys, values): (Vec<&str>, Vec<&str>) = annotations
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .unzip();
    sqlx::query!(
        r#"
        insert into tml_switchboard.job_annotations (job_id, key, value)
        select $1, key, value from unnest($2::text[], $3::text[]) as a(key, value)
        "#,
        job_id,
        &keys as &[&str],
        &values as &[&str],
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// What [`apply_changes`] did: the prior and new value (`None` for absent) of
/// each key it actually changed, and how many annotations the job has now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AnnotationsChange {
    pub old: BTreeMap<String, Option<String>>,
    pub new: BTreeMap<String, Option<String>>,
    pub count: i64,
}

/// Merge `changes` into a job's annotations, within the caller's transaction:
/// `Some` sets a key, `None` removes it. The caller must hold the job row's
/// lock, which serializes concurrent patches of the same job's annotations.
pub async fn apply_changes(
    job_id: Uuid,
    changes: &BTreeMap<String, Option<String>>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<AnnotationsChange, sqlx::Error> {
    let keys: Vec<&str> = changes.keys().map(String::as_str).collect();
    let before: BTreeMap<String, String> = sqlx::query!(
        r#"select key, value from tml_switchboard.job_annotations
           where job_id = $1 and key = any($2::text[])"#,
        job_id,
        &keys as &[&str],
    )
    .fetch_all(&mut **txn)
    .await?
    .into_iter()
    .map(|r| (r.key, r.value))
    .collect();

    let removed: Vec<&str> = changes
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.as_str())
        .collect();
    let (set_keys, set_values): (Vec<&str>, Vec<&str>) = changes
        .iter()
        .filter_map(|(key, value)| Some((key.as_str(), value.as_deref()?)))
        .unzip();

    sqlx::query!(
        r#"delete from tml_switchboard.job_annotations
           where job_id = $1 and key = any($2::text[])"#,
        job_id,
        &removed as &[&str],
    )
    .execute(&mut **txn)
    .await?;
    sqlx::query!(
        r#"
        insert into tml_switchboard.job_annotations (job_id, key, value)
        select $1, key, value from unnest($2::text[], $3::text[]) as a(key, value)
        on conflict (job_id, key) do update set value = excluded.value
        "#,
        job_id,
        &set_keys as &[&str],
        &set_values as &[&str],
    )
    .execute(&mut **txn)
    .await?;

    let count = sqlx::query_scalar!(
        r#"select count(*) as "count!" from tml_switchboard.job_annotations where job_id = $1"#,
        job_id,
    )
    .fetch_one(&mut **txn)
    .await?;

    let mut change = AnnotationsChange {
        count,
        ..Default::default()
    };
    for (key, value) in changes {
        let old = before.get(key);
        if old != value.as_ref() {
            change.old.insert(key.clone(), old.cloned());
            change.new.insert(key.clone(), value.clone());
        }
    }
    Ok(change)
}
