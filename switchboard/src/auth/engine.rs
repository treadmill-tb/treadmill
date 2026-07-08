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

/// The seeded `everyone` subject (see `SCHEMA.sql`): the public. Every subject is
/// implicitly a member of it (`principals()` always unions it in), so granting it
/// a permission on a resource makes that permission public. It is the uniform
/// replacement for per-entity `public` boolean columns.
pub const EVERYONE_SUBJECT_ID: Uuid = Uuid::from_u128(4);

/// The seeded `anonymous` `system` subject (see `SCHEMA.sql`). Used as the audit
/// `actor_id` for events raised about an unauthenticated external party who has
/// no local subject -- e.g. a login denied by the admission gate before any user
/// record exists. It never logs in and owns nothing.
pub const ANONYMOUS_SUBJECT_ID: Uuid = Uuid::from_u128(3);

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

/// A permission on an image set; mirrors `tml_switchboard.image_set_permission`
/// and the API's [`ApiImageSetPermission`].
///
/// [`ApiImageSetPermission`]:
///     treadmill_rs::api::switchboard::images::ImageSetPermission
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSetPermission {
    Use,
    Manage,
}
impl ImageSetPermission {
    pub const ALL: &'static [ImageSetPermission] =
        &[ImageSetPermission::Use, ImageSetPermission::Manage];

    /// The textual value as stored in the `image_set_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageSetPermission::Use => "use",
            ImageSetPermission::Manage => "manage",
        }
    }

    /// Parse the textual value as stored in the `image_set_permission` enum;
    /// the inverse of [`as_str`](Self::as_str). `None` for an unrecognized string.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "use" => Some(ImageSetPermission::Use),
            "manage" => Some(ImageSetPermission::Manage),
            _ => None,
        }
    }
}
impl From<treadmill_rs::api::switchboard::images::ImageSetPermission> for ImageSetPermission {
    fn from(p: treadmill_rs::api::switchboard::images::ImageSetPermission) -> Self {
        use treadmill_rs::api::switchboard::images::ImageSetPermission as Api;
        match p {
            Api::Use => ImageSetPermission::Use,
            Api::Manage => ImageSetPermission::Manage,
        }
    }
}

/// Whether `subject_id` may exercise `permission` on the image set.
///
/// Authorized iff the subject is a global admin, owns the set (directly or via
/// a group it transitively joins), or holds a matching grant — the standard
/// ownership ∨ grant disjunction over `principals()`. A "public" set is simply
/// one that grants the `everyone` subject `use`; since `principals()` folds
/// `everyone` in, that case needs no special handling here. The owner implicitly
/// holds every permission, so a `manage` query passes for the owner even with no
/// grant.
pub async fn can_access_image_set(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    set_id: Uuid,
    permission: ImageSetPermission,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "with principals(id) as ( \
             select id from tml_switchboard.principals($1::uuid) \
         ) \
         select ( \
             exists (select 1 from principals where id = $4::uuid) \
             or exists ( \
                 select 1 from tml_switchboard.image_sets g \
                 join principals p on g.owner_subject = p.id \
                 where g.id = $2::uuid \
             ) \
             or exists ( \
                 select 1 from tml_switchboard.image_set_grants gr \
                 join principals p on gr.subject_id = p.id \
                 where gr.set_id = $2::uuid and gr.permission::text = $3 \
             ) \
         ) as \"authorized!\"",
        subject_id,
        set_id,
        permission.as_str(),
        ADMINS_GROUP_ID,
    )
    .fetch_one(conn)
    .await
}

/// A permission on an image source; mirrors `tml_switchboard.image_source_permission`
/// and the API's [`ApiImageSourcePermission`].
///
/// [`ApiImageSourcePermission`]:
///     treadmill_rs::api::switchboard::images::ImageSourcePermission
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSourcePermission {
    Use,
    Manage,
}
impl ImageSourcePermission {
    pub const ALL: &'static [ImageSourcePermission] =
        &[ImageSourcePermission::Use, ImageSourcePermission::Manage];

    /// The textual value as stored in the `image_source_permission` enum.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageSourcePermission::Use => "use",
            ImageSourcePermission::Manage => "manage",
        }
    }

    /// Parse the textual value as stored in the `image_source_permission` enum;
    /// the inverse of [`as_str`](Self::as_str). `None` for an unrecognized string.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "use" => Some(ImageSourcePermission::Use),
            "manage" => Some(ImageSourcePermission::Manage),
            _ => None,
        }
    }
}
impl From<treadmill_rs::api::switchboard::images::ImageSourcePermission> for ImageSourcePermission {
    fn from(p: treadmill_rs::api::switchboard::images::ImageSourcePermission) -> Self {
        use treadmill_rs::api::switchboard::images::ImageSourcePermission as Api;
        match p {
            Api::Use => ImageSourcePermission::Use,
            Api::Manage => ImageSourcePermission::Manage,
        }
    }
}

/// Whether `subject_id` may exercise `permission` on the image source.
///
/// Authorized iff the subject is a global admin, owns the source (directly or via
/// a group it transitively joins), or holds a matching grant — the standard
/// ownership ∨ grant disjunction over `principals()`. A "public" source is simply
/// one that grants the `everyone` subject `use`; since `principals()` folds
/// `everyone` in, that case needs no special handling here. The owner implicitly
/// holds every permission. Mirrors [`can_access_image_set`].
pub async fn can_access_image_source(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    source_id: Uuid,
    permission: ImageSourcePermission,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        "with principals(id) as ( \
             select id from tml_switchboard.principals($1::uuid) \
         ) \
         select ( \
             exists (select 1 from principals where id = $4::uuid) \
             or exists ( \
                 select 1 from tml_switchboard.image_sources s \
                 join principals p on s.owner_subject = p.id \
                 where s.id = $2::uuid \
             ) \
             or exists ( \
                 select 1 from tml_switchboard.image_source_grants gr \
                 join principals p on gr.subject_id = p.id \
                 where gr.source_id = $2::uuid and gr.permission::text = $3 \
             ) \
         ) as \"authorized!\"",
        subject_id,
        source_id,
        permission.as_str(),
        ADMINS_GROUP_ID,
    )
    .fetch_one(conn)
    .await
}

/// Returns the complete set of permissions `subject_id` holds on `source_id`.
/// Owner or global admin yields all permissions; a grant to the `everyone`
/// subject (folded in via `principals()`) confers "public" permissions on all.
/// Mirrors [`can_access_image_source`] but enumerates rather than testing one
/// permission (for surfacing the viewer's per-source permissions).
pub async fn image_source_permissions(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    source_id: Uuid,
) -> Result<Vec<ImageSourcePermission>, sqlx::Error> {
    let rows = sqlx::query_scalar!(
        r#"
        with principals(id) as (
             select id from tml_switchboard.principals($1::uuid)
         ),
         is_global_or_owner as (
             select (
                 exists (select 1 from principals where id = $3::uuid)
                 or exists (
                     select 1 from tml_switchboard.image_sources s
                     join principals p on s.owner_subject = p.id
                     where s.id = $2::uuid
                 )
             ) as is_auth
         )
         select '*' as "perm!" from is_global_or_owner where is_auth
         union
         select g.permission::text as "perm!"
         from tml_switchboard.image_source_grants g
         join principals p on g.subject_id = p.id
         where g.source_id = $2::uuid
           and not (select is_auth from is_global_or_owner)
        "#,
        subject_id,
        source_id,
        ADMINS_GROUP_ID,
    )
    .fetch_all(conn)
    .await?;

    if rows.iter().any(|s| s == "*") {
        Ok(ImageSourcePermission::ALL.to_vec())
    } else {
        Ok(rows
            .into_iter()
            .filter_map(|s: String| ImageSourcePermission::from_db_str(&s))
            .collect())
    }
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

/// Returns the complete set of permissions `subject_id` holds on `set_id`.
/// Owner or global admin yields all permissions; a grant to the `everyone`
/// subject (folded in via `principals()`) confers "public" permissions on all.
/// Mirrors [`can_access_image_set`] but enumerates rather than testing one
/// permission, for the audit feed's policy match.
pub async fn image_set_permissions(
    conn: impl PgExecutor<'_>,
    subject_id: Uuid,
    set_id: Uuid,
) -> Result<Vec<ImageSetPermission>, sqlx::Error> {
    let rows = sqlx::query_scalar!(
        r#"
        with principals(id) as (
             select id from tml_switchboard.principals($1::uuid)
         ),
         is_global_or_owner as (
             select (
                 exists (select 1 from principals where id = $3::uuid)
                 or exists (
                     select 1 from tml_switchboard.image_sets g
                     join principals p on g.owner_subject = p.id
                     where g.id = $2::uuid
                 )
             ) as is_auth
         )
         select '*' as "perm!" from is_global_or_owner where is_auth
         union
         select g.permission::text as "perm!"
         from tml_switchboard.image_set_grants g
         join principals p on g.subject_id = p.id
         where g.set_id = $2::uuid
           and not (select is_auth from is_global_or_owner)
        "#,
        subject_id,
        set_id,
        ADMINS_GROUP_ID,
    )
    .fetch_all(conn)
    .await?;

    if rows.iter().any(|s| s == "*") {
        Ok(ImageSetPermission::ALL.to_vec())
    } else {
        Ok(rows
            .into_iter()
            .filter_map(|s: String| ImageSetPermission::from_db_str(&s))
            .collect())
    }
}
