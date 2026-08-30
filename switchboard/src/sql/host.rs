use sqlx::PgExecutor;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::auth::token::SecurityToken;

#[derive(Debug)]
pub struct SqlHost {
    pub host_id: Uuid,
    pub name: String,
    pub current_job: Option<Uuid>,
    pub worker_instance_id: i64,
}

/// A host's operational fields for the `GET /hosts` listing. What the host *is*
/// comes from its spec; this excludes supervisor credentials and worker
/// bookkeeping.
#[derive(Debug)]
pub struct SqlHostListing {
    pub host_id: Uuid,
    pub name: String,
    pub maintenance: bool,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The hosts `subject` may `read`, with their operational fields, ordered by
/// `(name, host_id)`: `name` is free-form and non-unique, so it needs a
/// tiebreak to paginate stably.
///
/// Mirrors `can_access_host(subject, host, 'read')` in `src/auth/engine.rs`,
/// evaluated set-wise so the listing stays one query. A spec is readable by
/// anyone who can read its host, which is why the filter has to be here rather
/// than left to the caller.
pub async fn list_readable(
    subject_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Vec<SqlHostListing>, sqlx::Error> {
    sqlx::query_as!(
        SqlHostListing,
        r#"with principals (id) as (
               select id from tml_switchboard.principals($1::uuid)
           )
           select host_id, name, maintenance, last_seen_at
           from tml_switchboard.hosts h
           where exists (select 1 from principals where id = $2::uuid)
              or exists (select 1 from principals p where p.id = h.owner_id)
              or exists (
                     select 1
                     from tml_switchboard.host_grants g
                     join principals p on g.subject_id = p.id
                     where g.host_id = h.host_id and g.permission = 'read'
                 )
           order by name, host_id"#,
        subject_id,
        crate::auth::engine::ADMINS_GROUP_ID,
    )
    .fetch_all(conn)
    .await
}

pub async fn insert(
    host_id: Uuid,
    name: String,
    auth_token: SecurityToken,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.hosts (host_id, name, auth_token)
           values ($1, $2, $3)"#,
        host_id,
        name,
        auth_token.as_bytes(),
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

/// Read a host's maintenance flag under a row lock, or `None` if no such host
/// exists.
///
/// Paired with [`set_maintenance`] in one transaction so the audit event
/// records the value actually replaced.
pub async fn lock_maintenance(
    host_id: Uuid,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"select maintenance
           from tml_switchboard.hosts
           where host_id = $1
           for update"#,
        host_id,
    )
    .fetch_optional(&mut **txn)
    .await
}

/// Set a host's maintenance flag.
///
/// Maintenance is operational state on the `hosts` row rather than a spec
/// field, so toggling it neither writes a spec revision nor requires one to
/// exist.
pub async fn set_maintenance(
    host_id: Uuid,
    maintenance: bool,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"update tml_switchboard.hosts
           set maintenance = $2
           where host_id = $1"#,
        host_id,
        maintenance,
    )
    .execute(&mut **txn)
    .await
    .map(|_| ())
}
