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
    pub const ALL: &'static [HostPermission] = &[
        HostPermission::Read,
        HostPermission::Start,
        HostPermission::Ssh,
        HostPermission::Manage,
    ];

    /// The textual value as stored in the `host_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            HostPermission::Read => "read",
            HostPermission::Start => "start",
            HostPermission::Ssh => "ssh",
            HostPermission::Manage => "manage",
        }
    }

    /// Parse the textual value as stored in the `host_permission` enum; the
    /// inverse of [`as_str`](Self::as_str). `None` for an unrecognized string.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "read" => Some(HostPermission::Read),
            "start" => Some(HostPermission::Start),
            "ssh" => Some(HostPermission::Ssh),
            "manage" => Some(HostPermission::Manage),
            _ => None,
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
    pub const ALL: &'static [JobPermission] = &[
        JobPermission::Read,
        JobPermission::Stop,
        JobPermission::Ssh,
        JobPermission::Manage,
    ];

    /// The textual value as stored in the `job_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            JobPermission::Read => "read",
            JobPermission::Stop => "stop",
            JobPermission::Ssh => "ssh",
            JobPermission::Manage => "manage",
        }
    }

    /// Parse the textual value as stored in the `job_permission` enum; the
    /// inverse of [`as_str`](Self::as_str). `None` for an unrecognized string.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "read" => Some(JobPermission::Read),
            "stop" => Some(JobPermission::Stop),
            "ssh" => Some(JobPermission::Ssh),
            "manage" => Some(JobPermission::Manage),
            _ => None,
        }
    }
}

/// Returns true if `subject_id` is a member of the global `admins` group.
pub async fn is_admin(conn: impl PgExecutor<'_>, subject_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "select exists(select 1 from tml_switchboard.principals($1::uuid) where id = $2::uuid) as \"is_admin!\"",
        subject_id,
        ADMINS_GROUP_ID,
    )
    .fetch_one(conn)
    .await
}
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

/// A permission on an image group; mirrors `tml_switchboard.image_group_permission`
/// and the API's [`ApiImageGroupPermission`].
///
/// [`ApiImageGroupPermission`]:
///     treadmill_rs::api::switchboard::images::ImageGroupPermission
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageGroupPermission {
    Use,
    Manage,
}
impl ImageGroupPermission {
    /// The textual value as stored in the `image_group_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageGroupPermission::Use => "use",
            ImageGroupPermission::Manage => "manage",
        }
    }
}
impl From<treadmill_rs::api::switchboard::images::ImageGroupPermission> for ImageGroupPermission {
    fn from(p: treadmill_rs::api::switchboard::images::ImageGroupPermission) -> Self {
        use treadmill_rs::api::switchboard::images::ImageGroupPermission as Api;
        match p {
            Api::Use => ImageGroupPermission::Use,
            Api::Manage => ImageGroupPermission::Manage,
        }
    }
}

/// Whether `subject_id` may exercise `permission` on the image group.
///
/// Authorized iff the subject is a global admin, owns the group (directly or via
/// a group it transitively joins), holds a matching grant — the standard
/// ownership ∨ grant disjunction over `principals()` — or, for `use` only, the
/// group is marked `public`. The owner implicitly holds every permission, so a
/// `manage` query passes for the owner even with no grant; `public` never
/// confers `manage`.
pub async fn can_access_image_group(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    group_id: Uuid,
    permission: ImageGroupPermission,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "with principals(id) as ( \
             select id from tml_switchboard.principals($1::uuid) \
         ) \
         select ( \
             exists (select 1 from principals where id = $4::uuid) \
             or exists ( \
                 select 1 from tml_switchboard.image_groups g \
                 join principals p on g.owner_subject = p.id \
                 where g.id = $2::uuid \
             ) \
             or exists ( \
                 select 1 from tml_switchboard.image_group_grants gr \
                 join principals p on gr.subject_id = p.id \
                 where gr.group_id = $2::uuid and gr.permission::text = $3 \
             ) \
             or ( \
                 $3 = 'use' \
                 and exists ( \
                     select 1 from tml_switchboard.image_groups g \
                     where g.id = $2::uuid and g.public \
                 ) \
             ) \
         ) as \"authorized!\"",
        subject_id,
        group_id,
        permission.as_str(),
        ADMINS_GROUP_ID,
    )
    .fetch_one(conn)
    .await
}

/// Returns the complete set of permissions `subject_id` holds on `host_id`.
/// If the subject is an owner or global admin, this returns all permissions.
pub async fn host_permissions(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    host_id: Uuid,
) -> Result<Vec<HostPermission>, sqlx::Error> {
    let rows = sqlx::query_scalar!(
        r#"
        with principals(id) as (
             select id from tml_switchboard.principals($1::uuid)
         ),
         is_global_or_owner as (
             select (
                 exists (select 1 from principals where id = $3::uuid)
                 or exists (
                     select 1 from tml_switchboard.hosts h
                     join principals p on h.owner_id = p.id
                     where h.host_id = $2::uuid
                 )
             ) as is_auth
         )
         select '*' as "perm!" from is_global_or_owner where is_auth
         union
         select g.permission::text as "perm!"
         from tml_switchboard.host_grants g
         join principals p on g.subject_id = p.id
         where g.host_id = $2::uuid
           and not (select is_auth from is_global_or_owner)
        "#,
        subject_id,
        host_id,
        ADMINS_GROUP_ID,
    )
    .fetch_all(conn)
    .await?;

    if rows.iter().any(|s| s == "*") {
        Ok(HostPermission::ALL.to_vec())
    } else {
        Ok(rows
            .into_iter()
            .filter_map(|s: String| HostPermission::from_db_str(&s))
            .collect())
    }
}

/// Returns the complete set of permissions `subject_id` holds on `job_id`.
/// If the subject is an owner or global admin, this returns all permissions.
pub async fn job_permissions(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    job_id: Uuid,
) -> Result<Vec<JobPermission>, sqlx::Error> {
    let rows = sqlx::query_scalar!(
        r#"
        with principals(id) as (
             select id from tml_switchboard.principals($1::uuid)
         ),
         is_global_or_owner as (
             select (
                 exists (select 1 from principals where id = $3::uuid)
                 or exists (
                     select 1 from tml_switchboard.jobs j
                     join principals p on j.owner_id = p.id
                     where j.job_id = $2::uuid
                 )
             ) as is_auth
         )
         select '*' as "perm!" from is_global_or_owner where is_auth
         union
         select g.permission::text as "perm!"
         from tml_switchboard.job_grants g
         join principals p on g.subject_id = p.id
         where g.job_id = $2::uuid
           and not (select is_auth from is_global_or_owner)
        "#,
        subject_id,
        job_id,
        ADMINS_GROUP_ID,
    )
    .fetch_all(conn)
    .await?;

    if rows.iter().any(|s| s == "*") {
        Ok(JobPermission::ALL.to_vec())
    } else {
        Ok(rows
            .into_iter()
            .filter_map(|s: String| JobPermission::from_db_str(&s))
            .collect())
    }
}
