//! Data access for the image catalog (`images`, `image_locations`,
//! `image_groups`, `image_group_generations`, `image_group_members`,
//! `image_group_grants`).
//!
//! The catalog stores only references for concrete images — a content-addressed
//! digest and the `{registry, repository}` locations that serve it — never image
//! bytes (see `doc/oci-image-migration-plan.md` §5.3/§8). Image *groups* are
//! mutable, named, generationed entities. These helpers back the
//! registration/list routes ([`crate::routes::images`]) and the dispatch-time
//! image resolver ([`crate::sql::job`]).

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;
use uuid::Uuid;

/// A registered image, identified by its OCI manifest digest.
#[derive(Debug, Clone)]
pub struct ImageRecord {
    pub id: Uuid,
    pub manifest_digest: String,
    pub artifact_type: String,
    pub owner_subject: Option<Uuid>,
    pub label: Option<String>,
    pub attrs: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// One registry location an image's bytes can be pulled from.
#[derive(Debug, Clone)]
pub struct LocationRecord {
    pub registry: String,
    pub repository: String,
    pub status: String,
}

/// A named, mutable image group.
#[derive(Debug, Clone)]
pub struct GroupRecord {
    pub id: Uuid,
    pub name: String,
    pub owner_subject: Option<Uuid>,
    pub label: Option<String>,
    /// When set, every subject implicitly holds `use` on the group.
    pub public: bool,
    pub created_at: DateTime<Utc>,
}

/// One member of a generation, as consumed by the matcher. `required_host_tags`
/// is the member's host-tag eligibility set; `index` is its explicit array
/// position (for deterministic tie-breaks).
#[derive(Debug, Clone)]
pub struct GroupMemberRecord {
    pub image_id: Uuid,
    pub manifest_digest: String,
    pub required_host_tags: Vec<String>,
    pub index: i32,
}

/// One immutable generation (membership snapshot) of a group.
#[derive(Debug, Clone)]
pub struct GenerationRecord {
    pub generation: i32,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// One grant on an image group.
#[derive(Debug, Clone)]
pub struct GroupGrantRecord {
    pub subject_id: Uuid,
    pub permission: String,
}

// -- images ---------------------------------------------------------------------

/// Look an image up by its manifest digest.
pub async fn fetch_by_digest(
    conn: impl PgExecutor<'_>,
    manifest_digest: &str,
) -> Result<Option<ImageRecord>, sqlx::Error> {
    sqlx::query_as!(
        ImageRecord,
        r#"select id, manifest_digest, artifact_type, owner_subject, label,
                  attrs as "attrs: serde_json::Value", created_at
           from tml_switchboard.images where manifest_digest = $1"#,
        manifest_digest,
    )
    .fetch_optional(conn)
    .await
}

/// Look an image up by its catalog id.
pub async fn fetch_by_id(
    conn: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<ImageRecord>, sqlx::Error> {
    sqlx::query_as!(
        ImageRecord,
        r#"select id, manifest_digest, artifact_type, owner_subject, label,
                  attrs as "attrs: serde_json::Value", created_at
           from tml_switchboard.images where id = $1"#,
        id,
    )
    .fetch_optional(conn)
    .await
}

/// Insert a new image row.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    manifest_digest: &str,
    artifact_type: &str,
    owner_subject: Uuid,
    label: Option<&str>,
    attrs: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.images
             (id, manifest_digest, artifact_type, owner_subject, label, attrs)
           values ($1, $2, $3, $4, $5, $6)"#,
        id,
        manifest_digest,
        artifact_type,
        owner_subject,
        label,
        attrs,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Add a location for an image, ignoring a duplicate `(registry, repository)`.
pub async fn upsert_location(
    conn: impl PgExecutor<'_>,
    image_id: Uuid,
    registry: &str,
    repository: &str,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_locations
             (image_id, registry, repository, status)
           values ($1, $2, $3, $4)
           on conflict (image_id, registry, repository) do nothing"#,
        image_id,
        registry,
        repository,
        status,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// All locations for an image, canonical/system preferred over external.
pub async fn locations_for_image(
    conn: impl PgExecutor<'_>,
    image_id: Uuid,
) -> Result<Vec<LocationRecord>, sqlx::Error> {
    sqlx::query_as!(
        LocationRecord,
        r#"select registry, repository, status
           from tml_switchboard.image_locations
           where image_id = $1
           order by case status
                      when 'system' then 0
                      when 'canonical' then 1
                      else 2
                    end,
                    added_at"#,
        image_id,
    )
    .fetch_all(conn)
    .await
}

/// Images visible to `subject` (owned by it or a group it transitively joins).
pub async fn list_owned(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
) -> Result<Vec<ImageRecord>, sqlx::Error> {
    sqlx::query_as!(
        ImageRecord,
        r#"select i.id, i.manifest_digest, i.artifact_type, i.owner_subject, i.label,
                  i.attrs as "attrs: serde_json::Value", i.created_at
           from tml_switchboard.images i
           join tml_switchboard.principals($1) p on p.id = i.owner_subject
           order by i.created_at"#,
        subject,
    )
    .fetch_all(conn)
    .await
}

// -- image groups ---------------------------------------------------------------

/// Create a new, empty named image group.
pub async fn create_group(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    name: &str,
    owner_subject: Uuid,
    label: Option<&str>,
    public: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_groups
             (id, name, owner_subject, label, public)
           values ($1, $2, $3, $4, $5)"#,
        id,
        name,
        owner_subject,
        label,
        public,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Set (or clear) a group's `public` flag, granting/revoking the implicit `use`
/// permission for every subject.
pub async fn set_group_public(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    public: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"update tml_switchboard.image_groups set public = $2 where id = $1"#,
        id,
        public,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Look a group up by its stable id.
pub async fn fetch_group_by_id(
    conn: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<GroupRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupRecord,
        r#"select id, name, owner_subject, label, public, created_at
           from tml_switchboard.image_groups where id = $1"#,
        id,
    )
    .fetch_optional(conn)
    .await
}

/// Look a group up by its unique name.
pub async fn fetch_group_by_name(
    conn: impl PgExecutor<'_>,
    name: &str,
) -> Result<Option<GroupRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupRecord,
        r#"select id, name, owner_subject, label, public, created_at
           from tml_switchboard.image_groups where name = $1"#,
        name,
    )
    .fetch_optional(conn)
    .await
}

/// Groups visible to `subject`: those it owns (directly or via a group it
/// transitively joins) plus every `public` group.
pub async fn list_owned_groups(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
) -> Result<Vec<GroupRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupRecord,
        r#"select g.id, g.name, g.owner_subject, g.label, g.public, g.created_at
           from tml_switchboard.image_groups g
           where g.public
              or exists (
                  select 1 from tml_switchboard.principals($1) p
                  where p.id = g.owner_subject
              )
           order by g.created_at"#,
        subject,
    )
    .fetch_all(conn)
    .await
}

/// The group's latest (highest) generation number, or `None` if it has none yet.
pub async fn latest_generation(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
) -> Result<Option<u32>, sqlx::Error> {
    let max: Option<i32> = sqlx::query_scalar!(
        r#"select max(generation) as "max"
           from tml_switchboard.image_group_generations
           where group_id = $1"#,
        group_id,
    )
    .fetch_one(conn)
    .await?;
    Ok(max.map(|g| g as u32))
}

/// Append a new full-replacement generation to a group, returning its number.
///
/// Allocation is serialized per-group by a transaction-scoped advisory lock
/// (mirrors `group_members_no_cycle`), so concurrent creators cannot collide on
/// the same `max+1`. `members` are `(image_id, required_host_tags, index)`; the
/// caller assigns `index` from request array order. Must run inside a transaction
/// (the advisory lock is `xact`-scoped).
pub async fn create_generation(
    tx: &mut sqlx::PgConnection,
    group_id: Uuid,
    created_by: Uuid,
    members: &[(Uuid, Vec<String>, i32)],
) -> Result<u32, sqlx::Error> {
    sqlx::query!(
        r#"select pg_advisory_xact_lock(hashtext('image_group_gen:' || $1::text))"#,
        group_id.to_string(),
    )
    .execute(&mut *tx)
    .await?;

    let next: i32 = sqlx::query_scalar!(
        r#"select coalesce(max(generation), 0) + 1 as "next!"
           from tml_switchboard.image_group_generations
           where group_id = $1"#,
        group_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query!(
        r#"insert into tml_switchboard.image_group_generations
             (group_id, generation, created_by)
           values ($1, $2, $3)"#,
        group_id,
        next,
        created_by,
    )
    .execute(&mut *tx)
    .await?;

    for (image_id, required_host_tags, index) in members {
        sqlx::query!(
            r#"insert into tml_switchboard.image_group_members
                 (group_id, generation, image_id, required_host_tags, "index")
               values ($1, $2, $3, $4, $5)"#,
            group_id,
            next,
            image_id,
            required_host_tags.as_slice(),
            index,
        )
        .execute(&mut *tx)
        .await?;
    }

    Ok(next as u32)
}

/// Fetch one generation's metadata row, if it exists.
pub async fn fetch_generation(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
    generation: u32,
) -> Result<Option<GenerationRecord>, sqlx::Error> {
    sqlx::query_as!(
        GenerationRecord,
        r#"select generation, created_by, created_at
           from tml_switchboard.image_group_generations
           where group_id = $1 and generation = $2"#,
        group_id,
        generation as i32,
    )
    .fetch_optional(conn)
    .await
}

/// All members of one generation, joined to their image digest, ordered by
/// `index` so matcher tie-breaks are deterministic.
pub async fn members_for_generation(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
    generation: u32,
) -> Result<Vec<GroupMemberRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupMemberRecord,
        r#"select m.image_id, i.manifest_digest,
                  m.required_host_tags, m."index"
           from tml_switchboard.image_group_members m
           join tml_switchboard.images i on i.id = m.image_id
           where m.group_id = $1 and m.generation = $2
           order by m."index""#,
        group_id,
        generation as i32,
    )
    .fetch_all(conn)
    .await
}

// -- image-group grants ---------------------------------------------------------

/// Grant `permission` ("use" / "manage") on a group to a subject; idempotent.
pub async fn grant_image_group(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
    subject_id: Uuid,
    permission: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_group_grants
             (group_id, subject_id, permission)
           values ($1, $2, $3::text::tml_switchboard.image_group_permission)
           on conflict (group_id, subject_id, permission) do nothing"#,
        group_id,
        subject_id,
        permission,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Revoke a single `(subject, permission)` grant on a group. Returns whether a
/// row was removed.
pub async fn revoke_image_group(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
    subject_id: Uuid,
    permission: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"delete from tml_switchboard.image_group_grants
           where group_id = $1 and subject_id = $2
             and permission = $3::text::tml_switchboard.image_group_permission
           returning group_id"#,
        group_id,
        subject_id,
        permission,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// All grants on a group.
pub async fn list_image_group_grants(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
) -> Result<Vec<GroupGrantRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupGrantRecord,
        r#"select subject_id, permission::text as "permission!"
           from tml_switchboard.image_group_grants
           where group_id = $1
           order by granted_at"#,
        group_id,
    )
    .fetch_all(conn)
    .await
}
