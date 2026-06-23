use sqlx::PgExecutor;
use std::collections::BTreeSet;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::SqlSshEndpoint;
use crate::auth::token::SecurityToken;

#[derive(Debug)]
pub struct SqlHost {
    pub host_id: Uuid,
    pub name: String,
    pub tags: Vec<String>,
    pub ssh_endpoints: Vec<SqlSshEndpoint>,
    pub current_job: Option<Uuid>,
    pub worker_instance_id: i64,
}

/// One target (DUT) attached to a host, with its opaque tag set.
#[derive(Debug)]
pub struct SqlHostTarget {
    pub target_id: Uuid,
    pub tags: Vec<String>,
}

/// A host's user-facing fields for the `GET /hosts` listing: identity, opaque
/// tags, and liveness heartbeat. Excludes supervisor credentials and worker
/// bookkeeping carried on the row.
#[derive(Debug)]
pub struct SqlHostListing {
    pub host_id: Uuid,
    pub name: String,
    pub tags: Vec<String>,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A target (DUT) row carrying its owning host, for assembling the per-host
/// target lists of the `GET /hosts` listing in one query.
#[derive(Debug)]
pub struct SqlHostTargetListing {
    pub host_id: Uuid,
    pub name: String,
    pub tags: Vec<String>,
}

/// List all hosts' user-facing fields, ordered by name for a stable picker.
pub async fn list_for_listing(
    conn: impl PgExecutor<'_>,
) -> Result<Vec<SqlHostListing>, sqlx::Error> {
    sqlx::query_as!(
        SqlHostListing,
        r#"select host_id, name, tags, last_seen_at
           from tml_switchboard.hosts
           order by name, host_id"#,
    )
    .fetch_all(conn)
    .await
}

/// All targets (DUTs) across every host, ordered by `(host_id, name)` so a
/// caller can group them into per-host lists in a single pass.
pub async fn list_all_targets(
    conn: impl PgExecutor<'_>,
) -> Result<Vec<SqlHostTargetListing>, sqlx::Error> {
    sqlx::query_as!(
        SqlHostTargetListing,
        r#"select host_id, name, tags
           from tml_switchboard.host_targets
           order by host_id, name"#,
    )
    .fetch_all(conn)
    .await
}

/// All targets (DUTs) wired to a host, for the scheduler's DUT-requirement
/// match. Ordered by `target_id` for deterministic matching.
pub async fn targets_for_host(
    host_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Vec<SqlHostTarget>, sqlx::Error> {
    sqlx::query_as!(
        SqlHostTarget,
        r#"select target_id, tags
           from tml_switchboard.host_targets
           where host_id = $1
           order by target_id"#,
        host_id,
    )
    .fetch_all(conn)
    .await
}

pub async fn insert(
    host_id: Uuid,
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
            tml_switchboard.hosts
        (
            host_id,
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
        host_id,
        name,
        auth_token.as_bytes(),
        tag_vec.as_slice(),
        ssh_endpoints.as_slice() as &[SqlSshEndpoint],
    )
    .execute(conn)
    .await
    .map(|_| ())
}

pub async fn fetch_all_hosts(conn: impl PgExecutor<'_>) -> Result<Vec<SqlHost>, sqlx::Error> {
    sqlx::query_as!(
        SqlHost,
        r#"
        SELECT
            host_id,
            name,
            tags,
            ssh_endpoints as "ssh_endpoints: _",
            current_job,
            worker_instance_id
        FROM
            tml_switchboard.hosts
        "#
    )
    .fetch_all(conn)
    .await
}

/// Authenticate the supervisor process connecting to drive `host_id`.
///
/// The auth_token lives on the host row (one supervisor per host); this checks
/// the presented token against that record in constant time.
pub async fn try_authenticate_for_host(
    host_id: Uuid,
    auth_token: SecurityToken,
    conn: impl PgExecutor<'_>,
) -> Result<bool, sqlx::Error> {
    let maybe_record = sqlx::query!(
        r#"
        SELECT
            auth_token
        FROM
            tml_switchboard.hosts
        WHERE
            host_id = $1
        LIMIT 1;
        "#,
        host_id,
    )
    .fetch_optional(conn)
    .await?;

    let (flag, token_vec) = match maybe_record {
        Some(token_vec) => (subtle::Choice::from(1), token_vec.auth_token),
        None => (subtle::Choice::from(0), vec![0u8; 32]),
    };

    let sec_token =
        SecurityToken::try_from(token_vec).expect("stored auth token in database is invalid");

    let result = bool::from(sec_token.ct_eq(&auth_token) & ({ flag }));

    Ok(result)
}

pub async fn increment_worker_instance_id(
    host_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<i64, sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE
            tml_switchboard.hosts
        SET
            worker_instance_id = worker_instance_id + 1
        WHERE
            host_id = $1
        RETURNING
            worker_instance_id
        "#,
        host_id,
    )
    .fetch_one(conn)
    .await
    .map(|record| record.worker_instance_id)
}

/// Read a host's current job assignment (`hosts.current_job`).
///
/// Reconciliation calls this inside the worker's `with_txn`, after the row has
/// already been locked by [`lock_and_get_current_worker`], so the value is read
/// under the same transaction that performs any resulting state transition.
pub async fn fetch_current_job(
    host_id: Uuid,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query!(
        r#"
        SELECT
            current_job
        FROM
            tml_switchboard.hosts
        WHERE
            host_id = $1
        "#,
        host_id,
    )
    .fetch_one(&mut **txn)
    .await
    .map(|record| record.current_job)
}

/// Release a host's job assignment pointer (`hosts.current_job = NULL`), guarded
/// on it still pointing at `job_id`.
///
/// Unlike the `sql::job::finalize_*` helpers, this does **not** touch the job
/// row: it is for the case where the job is *already* finalized but the host
/// pointer was never released — the job reached a terminal state out-of-band
/// (e.g. finalized via a `SupervisorJobEvent::Error`, or a normal `Terminated`
/// whose ack is still in flight) and the supervisor has since confirmed it no
/// longer holds the job. Reconcile calls this only once the reported status
/// shows the job is gone, so `hosts.current_job` stays a faithful mirror of what
/// the supervisor actually holds.
///
/// Idempotent: the `current_job = job_id` guard makes a replay a no-op and
/// prevents clobbering a newer assignment. Must run inside the worker's
/// `with_txn` guard (like the other host mutators here).
pub async fn release_job_assignment(
    host_id: Uuid,
    job_id: Uuid,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE tml_switchboard.hosts
        SET current_job = NULL
        WHERE host_id = $1 AND current_job = $2
        "#,
        host_id,
        job_id,
    )
    .execute(&mut **txn)
    .await
    .map(|_| ())
}

/// Refresh the host's liveness heartbeat (`last_seen_at = now()`).
///
/// Call this only from inside the worker's `with_txn` guard, so the staleness
/// check has already confirmed this worker is still current — a superseded
/// worker's `with_txn` short-circuits before the closure runs and never reaches
/// here, so it cannot resurrect a host a newer worker now owns.
pub async fn touch_heartbeat(
    host_id: Uuid,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE tml_switchboard.hosts
        SET last_seen_at = now()
        WHERE host_id = $1
        "#,
        host_id,
    )
    .execute(&mut **txn)
    .await
    .map(|_| ())
}

/// Mark the host as not-live (`last_seen_at = NULL`), used when a worker
/// disconnects cleanly so the scheduler stops dispatching to it immediately
/// rather than waiting out the heartbeat staleness window.
///
/// Like [`touch_heartbeat`], this must run inside the worker's `with_txn`
/// guard: if the worker has been superseded, the guard rolls back before this
/// closure runs, so the clean-disconnect of an old worker can never clobber the
/// heartbeat a newer worker is keeping fresh.
pub async fn mark_dead(
    host_id: Uuid,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE tml_switchboard.hosts
        SET last_seen_at = NULL
        WHERE host_id = $1
        "#,
        host_id,
    )
    .execute(&mut **txn)
    .await
    .map(|_| ())
}

/// Acquire a row-level lock on the host record and return its current
/// `worker_instance_id`.
///
/// The `FOR UPDATE` clause blocks any concurrent transaction that wants the
/// same row lock — notably `increment_worker_instance_id` and other calls to
/// this function — until this transaction commits or rolls back. Worker
/// transactions call this as their first statement to serialize all writes
/// for a given host against worker takeover; the caller compares the returned
/// value against its own ID to detect being superseded.
pub async fn lock_and_get_current_worker(
    host_id: Uuid,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<i64, sqlx::Error> {
    sqlx::query!(
        r#"
        SELECT
            worker_instance_id
        FROM
            tml_switchboard.hosts
        WHERE
            host_id = $1
        FOR UPDATE
        "#,
        host_id,
    )
    .fetch_one(&mut **txn)
    .await
    .map(|record| record.worker_instance_id)
}
