//! Grant-based authorization engine.
//!
//! Answers the core question "may subject S exercise permission P on resource
//! R?" against the live database. A subject is authorized iff any of:
//!   - it is a member (directly or transitively) of the seeded `admins` group,
//!     which holds global authority over every resource (including orphans);
//!   - it, or any group it transitively belongs to, owns the resource (the
//!     owner implicitly holds every permission); or
//!   - it, or any group it transitively belongs to, holds a matching grant for
//!     the exact permission requested.
//!
//! "Principals" of S is the set {S} together with every group S reaches by
//! following `member_id -> group_id` edges. That transitive expansion lives in
//! the `tml_switchboard.principals(uuid)` SQL function (see `SCHEMA.sql`); each
//! query below wraps one call to it in a non-recursive CTE and checks ownership
//! and grants against that whole set in a single statement.

use sqlx::PgExecutor;
use uuid::Uuid;

/// The seeded group whose members wield global authority (see `SCHEMA.sql`).
pub const ADMINS_GROUP_ID: Uuid = Uuid::from_u128(1);

/// A permission on a host; mirrors `tml_switchboard.host_permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPermission {
    Read,
    Start,
    Ssh,
    Manage,
}
impl HostPermission {
    /// The textual value as stored in the `host_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            HostPermission::Read => "read",
            HostPermission::Start => "start",
            HostPermission::Ssh => "ssh",
            HostPermission::Manage => "manage",
        }
    }
}

/// A permission on a job; mirrors `tml_switchboard.job_permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPermission {
    Read,
    Stop,
    Ssh,
    Manage,
}
impl JobPermission {
    /// The textual value as stored in the `job_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            JobPermission::Read => "read",
            JobPermission::Stop => "stop",
            JobPermission::Ssh => "ssh",
            JobPermission::Manage => "manage",
        }
    }
}

/// Whether `subject_id` may exercise `permission` on the host.
pub async fn can_access_host(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    host_id: Uuid,
    permission: HostPermission,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "with principals(id) as ( \
             select id from tml_switchboard.principals($1::uuid) \
         ) \
         select ( \
             exists (select 1 from principals where id = $4::uuid) \
             or exists ( \
                 select 1 from tml_switchboard.hosts h \
                 join principals p on h.owner_id = p.id \
                 where h.host_id = $2::uuid \
             ) \
             or exists ( \
                 select 1 from tml_switchboard.host_grants g \
                 join principals p on g.subject_id = p.id \
                 where g.host_id = $2::uuid and g.permission::text = $3 \
             ) \
         ) as \"authorized!\"",
        subject_id,
        host_id,
        permission.as_str(),
        ADMINS_GROUP_ID,
    )
    .fetch_one(conn)
    .await
}

/// Whether `subject_id` may exercise `permission` on the job.
pub async fn can_access_job(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    job_id: Uuid,
    permission: JobPermission,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "with principals(id) as ( \
             select id from tml_switchboard.principals($1::uuid) \
         ) \
         select ( \
             exists (select 1 from principals where id = $4::uuid) \
             or exists ( \
                 select 1 from tml_switchboard.jobs j \
                 join principals p on j.owner_id = p.id \
                 where j.job_id = $2::uuid \
             ) \
             or exists ( \
                 select 1 from tml_switchboard.job_grants g \
                 join principals p on g.subject_id = p.id \
                 where g.job_id = $2::uuid and g.permission::text = $3 \
             ) \
         ) as \"authorized!\"",
        subject_id,
        job_id,
        permission.as_str(),
        ADMINS_GROUP_ID,
    )
    .fetch_one(conn)
    .await
}
