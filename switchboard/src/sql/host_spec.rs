//! Reads and writes of `host_specs`, the sole store of host descriptions.
//!
//! The table is append-only and the highest `revision` per host is that host's
//! current spec, so there is no current-value column and no update path: a
//! write is an insert at the next revision.

use sqlx::PgExecutor;
use treadmill_rs::host_spec::HostSpec;
use uuid::Uuid;

/// A stored revision, still in its written-under version.
#[derive(Debug, Clone)]
pub struct StoredSpec {
    pub host_id: Uuid,
    pub revision: i32,
    pub spec: HostSpec,
}

impl StoredSpec {
    /// Fold the document forward to the current version. Every read path goes
    /// through here, so nothing downstream sees an outdated version.
    pub fn normalize(self) -> treadmill_rs::host_spec::HostSpecV1 {
        self.spec.into_latest()
    }
}

/// A document that failed to deserialize, which can only mean a row was written
/// under a version this build no longer understands or was edited out of band.
#[derive(Debug, thiserror::Error)]
#[error("host {host_id} revision {revision} does not deserialize: {source}")]
pub struct MalformedSpec {
    pub host_id: Uuid,
    pub revision: i32,
    pub source: serde_json::Error,
}

fn decode(
    host_id: Uuid,
    revision: i32,
    spec: serde_json::Value,
) -> Result<StoredSpec, MalformedSpec> {
    serde_json::from_value(spec)
        .map(|spec| StoredSpec {
            host_id,
            revision,
            spec,
        })
        .map_err(|source| MalformedSpec {
            host_id,
            revision,
            source,
        })
}

/// The current spec of every host that has one.
///
/// `distinct on` with a descending revision walks the primary key backwards,
/// so this needs no extra index. The scheduler runs it once per pass rather
/// than once per (job, host) pair.
pub async fn current_for_all_hosts(
    conn: impl PgExecutor<'_>,
) -> Result<Vec<Result<StoredSpec, MalformedSpec>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"select distinct on (host_id) host_id, revision, spec
           from tml_switchboard.host_specs
           order by host_id, revision desc"#,
    )
    .fetch_all(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| decode(r.host_id, r.revision, r.spec))
        .collect())
}

/// One host's current spec, or `None` if it has never been described.
pub async fn current_for_host(
    host_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Option<Result<StoredSpec, MalformedSpec>>, sqlx::Error> {
    let row = sqlx::query!(
        r#"select revision, spec
           from tml_switchboard.host_specs
           where host_id = $1
           order by revision desc
           limit 1"#,
        host_id,
    )
    .fetch_optional(conn)
    .await?;

    Ok(row.map(|r| decode(host_id, r.revision, r.spec)))
}

/// Append `spec` as `revision`, attributed to `written_by`.
///
/// The caller supplies the revision it expects to create, which makes the
/// primary key the compare-and-swap: a racing writer that computed the same
/// number takes a unique violation instead of overwriting. `spec` is stored
/// exactly as submitted.
pub async fn insert_revision(
    host_id: Uuid,
    revision: i32,
    spec: &HostSpec,
    written_by: Option<Uuid>,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    let document = serde_json::to_value(spec).expect("host spec serializes");
    sqlx::query!(
        r#"insert into tml_switchboard.host_specs
             (host_id, revision, spec, spec_version, written_by)
           values ($1, $2, $3, $4, $5)"#,
        host_id,
        revision,
        document,
        spec.version(),
        written_by,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// The revision of a host's current spec, or `None` if it has never been
/// described (or does not exist).
///
/// Read by the conditional write path to compare against `If-Match`. It takes
/// no row lock: the primary key is the compare-and-swap, so a writer that
/// slips in between this read and the insert loses on the unique violation
/// rather than on a lock.
pub async fn current_revision(
    host_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"select max(revision) from tml_switchboard.host_specs where host_id = $1"#,
        host_id,
    )
    .fetch_one(conn)
    .await
}
