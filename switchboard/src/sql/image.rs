//! Data access for the image catalog (`images`, `image_locations`,
//! `image_groups`, `image_group_members`).
//!
//! The catalog stores only references — a content-addressed digest and the
//! `{registry, repository}` locations that serve it — never image bytes (see
//! `doc/oci-image-migration-plan.md` §5.3/§8). These helpers back the
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

/// A registered image group (an OCI index), identified by its index digest.
#[derive(Debug, Clone)]
pub struct GroupRecord {
    pub id: Uuid,
    pub index_digest: String,
    pub owner_subject: Option<Uuid>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A denormalized group member, as consumed by the matcher. `required_host_tags`
/// is the member's host-tag eligibility set; `position` is its order in the index
/// (for deterministic tie-breaks).
#[derive(Debug, Clone)]
pub struct GroupMemberRecord {
    pub image_id: Uuid,
    pub manifest_digest: String,
    pub required_host_tags: Vec<String>,
    pub position: i32,
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

/// Look a group up by its index digest.
pub async fn fetch_group_by_digest(
    conn: impl PgExecutor<'_>,
    index_digest: &str,
) -> Result<Option<GroupRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupRecord,
        r#"select id, index_digest, owner_subject, label, created_at
           from tml_switchboard.image_groups where index_digest = $1"#,
        index_digest,
    )
    .fetch_optional(conn)
    .await
}

/// Insert a new image-group row.
pub async fn insert_group(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    index_digest: &str,
    owner_subject: Uuid,
    label: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_groups
             (id, index_digest, owner_subject, label)
           values ($1, $2, $3, $4)"#,
        id,
        index_digest,
        owner_subject,
        label,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Drop all of a group's denormalized members (for a rebuild on re-registration).
pub async fn clear_members(conn: impl PgExecutor<'_>, group_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "delete from tml_switchboard.image_group_members where group_id = $1",
        group_id,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Insert one denormalized group member.
pub async fn insert_member(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
    image_id: Uuid,
    required_host_tags: &[String],
    position: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_group_members
             (group_id, image_id, required_host_tags, position)
           values ($1, $2, $3, $4)
           on conflict (group_id, image_id) do update set
             required_host_tags = excluded.required_host_tags,
             position = excluded.position"#,
        group_id,
        image_id,
        required_host_tags,
        position,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// All denormalized members of a group, joined to their image digest, in index
/// order (`position`) so matcher tie-breaks are deterministic.
pub async fn members_for_group(
    conn: impl PgExecutor<'_>,
    group_id: Uuid,
) -> Result<Vec<GroupMemberRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupMemberRecord,
        r#"select m.image_id, i.manifest_digest,
                  m.required_host_tags, m.position
           from tml_switchboard.image_group_members m
           join tml_switchboard.images i on i.id = m.image_id
           where m.group_id = $1
           order by m.position"#,
        group_id,
    )
    .fetch_all(conn)
    .await
}

/// Groups visible to `subject` (owned by it or a group it transitively joins).
pub async fn list_owned_groups(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
) -> Result<Vec<GroupRecord>, sqlx::Error> {
    sqlx::query_as!(
        GroupRecord,
        r#"select g.id, g.index_digest, g.owner_subject, g.label, g.created_at
           from tml_switchboard.image_groups g
           join tml_switchboard.principals($1) p on p.id = g.owner_subject
           order by g.created_at"#,
        subject,
    )
    .fetch_all(conn)
    .await
}
