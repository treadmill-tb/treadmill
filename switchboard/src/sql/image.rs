//! Data access for the image catalog (`images`, `image_sources`,
//! `image_source_grants`, `image_sets`, `image_set_generations`,
//! `image_set_members`, `image_set_grants`).
//!
//! The catalog stores only references for concrete images — a content-addressed
//! digest and the `{registry, repository}` sources that serve it — never image
//! bytes (see `doc/oci-image-migration-plan.md` §5.3/§8). An image is a non-owned
//! manifest identity; the *sources* behind it are the ownable, grantable entity.
//! Image *sets* are mutable, named, generationed entities. These helpers back
//! the registration/list routes ([`crate::routes::images`]) and the
//! dispatch-time image resolver ([`crate::sql::job`]).

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;
use uuid::Uuid;

/// A registered image: a non-owned manifest identity (see `SCHEMA.sql`). Owner
/// and ACL live on its [`SourceRecord`]s, not here.
#[derive(Debug, Clone)]
pub struct ImageRecord {
    pub id: Uuid,
    pub manifest_digest: String,
    pub artifact_type: String,
    pub title: Option<String>,
    pub attrs: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// One registry source an image's bytes can be pulled from — the ownable,
/// grantable, deletable catalog entity. `owner_subject` is NULL when orphaned.
#[derive(Debug, Clone)]
pub struct SourceRecord {
    pub id: Uuid,
    pub image_id: Uuid,
    pub registry: String,
    pub repository: String,
    pub status: String,
    pub owner_subject: Option<Uuid>,
}

/// One grant on an image source.
#[derive(Debug, Clone)]
pub struct SourceGrantRecord {
    pub subject_id: Uuid,
    pub permission: String,
}

/// Per-member usability of a generation: whether the viewer may use some source
/// of the member image, and whether *every* subject holding a `use` grant on the
/// set can (the owner-facing health signal). See [`generation_member_usability`].
#[derive(Debug, Clone)]
pub struct MemberUsability {
    pub image_id: Uuid,
    pub usable: bool,
    pub usable_by_grantees: bool,
}

/// A named, mutable image set.
///
/// A set is "public" iff it grants the well-known `everyone` subject `use`;
/// there is no dedicated flag on the row (see `SCHEMA.sql`).
#[derive(Debug, Clone)]
pub struct SetRecord {
    pub id: Uuid,
    pub name: String,
    pub owner_subject: Option<Uuid>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One member of a generation, as consumed by the matcher. `required_host_tags`
/// is the member's host-tag eligibility set; `index` is its explicit array
/// position (for deterministic tie-breaks).
#[derive(Debug, Clone)]
pub struct SetMemberRecord {
    pub image_id: Uuid,
    pub manifest_digest: String,
    pub required_host_tags: Vec<String>,
    pub index: i32,
}

/// One immutable generation (membership snapshot) of a set.
#[derive(Debug, Clone)]
pub struct GenerationRecord {
    pub generation: i32,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// One grant on an image set.
#[derive(Debug, Clone)]
pub struct SetGrantRecord {
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
        r#"select id, manifest_digest, artifact_type, title,
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
        r#"select id, manifest_digest, artifact_type, title,
                  attrs as "attrs: serde_json::Value", created_at
           from tml_switchboard.images where id = $1"#,
        id,
    )
    .fetch_optional(conn)
    .await
}

/// Insert a new (non-owned) image row.
pub async fn insert(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    manifest_digest: &str,
    artifact_type: &str,
    title: Option<&str>,
    attrs: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.images
             (id, manifest_digest, artifact_type, title, attrs)
           values ($1, $2, $3, $4, $5)"#,
        id,
        manifest_digest,
        artifact_type,
        title,
        attrs,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

// -- image sources --------------------------------------------------------------

/// Add a source for an image owned by `owner`, ignoring a duplicate
/// `(registry, repository)` (idempotent re-registration).
#[allow(clippy::too_many_arguments)]
pub async fn insert_source(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    image_id: Uuid,
    registry: &str,
    repository: &str,
    status: &str,
    owner: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_sources
             (id, image_id, registry, repository, status, owner_subject)
           values ($1, $2, $3, $4, $5, $6)
           on conflict (image_id, registry, repository) do nothing"#,
        id,
        image_id,
        registry,
        repository,
        status,
        owner,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Look a source up by its surrogate id.
pub async fn fetch_source(
    conn: impl PgExecutor<'_>,
    source_id: Uuid,
) -> Result<Option<SourceRecord>, sqlx::Error> {
    sqlx::query_as!(
        SourceRecord,
        r#"select id, image_id, registry, repository, status, owner_subject
           from tml_switchboard.image_sources where id = $1"#,
        source_id,
    )
    .fetch_optional(conn)
    .await
}

/// Delete a source by id. Sources are always deletable by owner/admins, even
/// when referenced by a generation. Returns whether a row was removed.
pub async fn delete_source(
    conn: impl PgExecutor<'_>,
    source_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"delete from tml_switchboard.image_sources
           where id = $1 returning id"#,
        source_id,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// All sources for an image, canonical/system preferred over external.
pub async fn sources_for_image(
    conn: impl PgExecutor<'_>,
    image_id: Uuid,
) -> Result<Vec<SourceRecord>, sqlx::Error> {
    sqlx::query_as!(
        SourceRecord,
        r#"select id, image_id, registry, repository, status, owner_subject
           from tml_switchboard.image_sources
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

/// The sources of an image that `owner` may `use`, canonical/system preferred.
/// The dispatch gate: only sources the job owner may reach are handed to the
/// supervisor (a source can be deleted or restricted between enqueue and
/// dispatch). Mirrors [`crate::auth::engine::can_access_image_source`]'s `use`
/// disjunction over `principals()` (with `everyone` folded in).
pub async fn usable_sources_for_image(
    conn: impl PgExecutor<'_>,
    image_id: Uuid,
    owner: Uuid,
) -> Result<Vec<SourceRecord>, sqlx::Error> {
    sqlx::query_as!(
        SourceRecord,
        r#"with principals(id) as (
               select id from tml_switchboard.principals($2)
           )
           select s.id, s.image_id, s.registry, s.repository, s.status, s.owner_subject
           from tml_switchboard.image_sources s
           where s.image_id = $1
             and (
                 exists (select 1 from principals where id = $3::uuid)
                 or exists (select 1 from principals p where p.id = s.owner_subject)
                 or exists (
                     select 1 from tml_switchboard.image_source_grants g
                     join principals p on g.subject_id = p.id
                     where g.source_id = s.id and g.permission = 'use'
                 )
             )
           order by case s.status
                      when 'system' then 0
                      when 'canonical' then 1
                      else 2
                    end,
                    s.added_at"#,
        image_id,
        owner,
        crate::auth::engine::ADMINS_GROUP_ID,
    )
    .fetch_all(conn)
    .await
}

/// Whether `subject` may `use` at least one source of `image_id`
/// (`tml_switchboard.image_source_usable`).
pub async fn image_source_usable(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
    image_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"select tml_switchboard.image_source_usable($1, $2) as "usable!""#,
        subject,
        image_id,
    )
    .fetch_one(conn)
    .await
}

// -- image-source grants --------------------------------------------------------

/// Grant `permission` ("use" / "manage") on a source to a subject; idempotent.
pub async fn grant_image_source(
    conn: impl PgExecutor<'_>,
    source_id: Uuid,
    subject_id: Uuid,
    permission: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_source_grants
             (source_id, subject_id, permission)
           values ($1, $2, $3::text::tml_switchboard.image_source_permission)
           on conflict (source_id, subject_id, permission) do nothing"#,
        source_id,
        subject_id,
        permission,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Revoke a single `(subject, permission)` grant on a source. Returns whether a
/// row was removed.
pub async fn revoke_image_source(
    conn: impl PgExecutor<'_>,
    source_id: Uuid,
    subject_id: Uuid,
    permission: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"delete from tml_switchboard.image_source_grants
           where source_id = $1 and subject_id = $2
             and permission = $3::text::tml_switchboard.image_source_permission
           returning source_id"#,
        source_id,
        subject_id,
        permission,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// All grants on a source.
pub async fn list_image_source_grants(
    conn: impl PgExecutor<'_>,
    source_id: Uuid,
) -> Result<Vec<SourceGrantRecord>, sqlx::Error> {
    sqlx::query_as!(
        SourceGrantRecord,
        r#"select subject_id, permission::text as "permission!"
           from tml_switchboard.image_source_grants
           where source_id = $1
           order by granted_at, subject_id, permission"#,
        source_id,
    )
    .fetch_all(conn)
    .await
}

/// Images the `subject` may use: those with at least one source it may `use`
/// (owned, `use`-granted, or public via the `everyone` grant), plus admin sees
/// all. Replaces the old ownership-based listing.
pub async fn list_usable_images(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
) -> Result<Vec<ImageRecord>, sqlx::Error> {
    sqlx::query_as!(
        ImageRecord,
        r#"select i.id, i.manifest_digest, i.artifact_type, i.title,
                  i.attrs as "attrs: serde_json::Value", i.created_at
           from tml_switchboard.images i
           where tml_switchboard.image_source_usable($1, i.id)
           order by i.created_at, i.id"#,
        subject,
    )
    .fetch_all(conn)
    .await
}

// -- image sets ---------------------------------------------------------------

/// Create a new, empty named image set.
pub async fn create_set(
    conn: impl PgExecutor<'_>,
    id: Uuid,
    name: &str,
    owner_subject: Uuid,
    label: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_sets
             (id, name, owner_subject, label)
           values ($1, $2, $3, $4)"#,
        id,
        name,
        owner_subject,
        label,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Look a set up by its stable id.
pub async fn fetch_set_by_id(
    conn: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<SetRecord>, sqlx::Error> {
    sqlx::query_as!(
        SetRecord,
        r#"select id, name, owner_subject, label, created_at
           from tml_switchboard.image_sets where id = $1"#,
        id,
    )
    .fetch_optional(conn)
    .await
}

/// Look a set up by its unique name.
pub async fn fetch_set_by_name(
    conn: impl PgExecutor<'_>,
    name: &str,
) -> Result<Option<SetRecord>, sqlx::Error> {
    sqlx::query_as!(
        SetRecord,
        r#"select id, name, owner_subject, label, created_at
           from tml_switchboard.image_sets where name = $1"#,
        name,
    )
    .fetch_optional(conn)
    .await
}

/// Sets visible to `subject`: those it owns (directly or via a group it
/// transitively joins) plus those granted to any of its principals. Because
/// `principals()` folds in the `everyone` subject, public sets (which grant
/// `everyone` `use`) are included for every caller.
pub async fn list_owned_sets(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
) -> Result<Vec<SetRecord>, sqlx::Error> {
    sqlx::query_as!(
        SetRecord,
        r#"select g.id, g.name, g.owner_subject, g.label, g.created_at
           from tml_switchboard.image_sets g
           where exists (
                  select 1 from tml_switchboard.principals($1) p
                  where p.id = g.owner_subject
              )
              or exists (
                  select 1
                  from tml_switchboard.image_set_grants gr
                  join tml_switchboard.principals($1) p on gr.subject_id = p.id
                  where gr.set_id = g.id
              )
           order by g.created_at, g.id"#,
        subject,
    )
    .fetch_all(conn)
    .await
}

/// The set's latest (highest) generation number, or `None` if it has none yet.
pub async fn latest_generation(
    conn: impl PgExecutor<'_>,
    set_id: Uuid,
) -> Result<Option<u32>, sqlx::Error> {
    let max: Option<i32> = sqlx::query_scalar!(
        r#"select max(generation) as "max"
           from tml_switchboard.image_set_generations
           where set_id = $1"#,
        set_id,
    )
    .fetch_one(conn)
    .await?;
    Ok(max.map(|g| g as u32))
}

/// Append a new full-replacement generation to a set, returning its number.
///
/// Allocation is serialized per-set by a transaction-scoped advisory lock
/// (mirrors `group_members_no_cycle`), so concurrent creators cannot collide on
/// the same `max+1`. `members` are `(image_id, required_host_tags, index)`; the
/// caller assigns `index` from request array order. Must run inside a transaction
/// (the advisory lock is `xact`-scoped).
pub async fn create_generation(
    tx: &mut sqlx::PgConnection,
    set_id: Uuid,
    created_by: Uuid,
    members: &[(Uuid, Vec<String>, i32)],
) -> Result<u32, sqlx::Error> {
    sqlx::query!(
        r#"select pg_advisory_xact_lock(hashtext('image_set_gen:' || $1::text))"#,
        set_id.to_string(),
    )
    .execute(&mut *tx)
    .await?;

    let next: i32 = sqlx::query_scalar!(
        r#"select coalesce(max(generation), 0) + 1 as "next!"
           from tml_switchboard.image_set_generations
           where set_id = $1"#,
        set_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query!(
        r#"insert into tml_switchboard.image_set_generations
             (set_id, generation, created_by)
           values ($1, $2, $3)"#,
        set_id,
        next,
        created_by,
    )
    .execute(&mut *tx)
    .await?;

    for (image_id, required_host_tags, index) in members {
        sqlx::query!(
            r#"insert into tml_switchboard.image_set_members
                 (set_id, generation, image_id, required_host_tags, "index")
               values ($1, $2, $3, $4, $5)"#,
            set_id,
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
    set_id: Uuid,
    generation: u32,
) -> Result<Option<GenerationRecord>, sqlx::Error> {
    sqlx::query_as!(
        GenerationRecord,
        r#"select generation, created_by, created_at
           from tml_switchboard.image_set_generations
           where set_id = $1 and generation = $2"#,
        set_id,
        generation as i32,
    )
    .fetch_optional(conn)
    .await
}

/// All members of one generation, joined to their image digest, ordered by
/// `index` so matcher tie-breaks are deterministic.
pub async fn members_for_generation(
    conn: impl PgExecutor<'_>,
    set_id: Uuid,
    generation: u32,
) -> Result<Vec<SetMemberRecord>, sqlx::Error> {
    sqlx::query_as!(
        SetMemberRecord,
        r#"select m.image_id, i.manifest_digest,
                  m.required_host_tags, m."index"
           from tml_switchboard.image_set_members m
           join tml_switchboard.images i on i.id = m.image_id
           where m.set_id = $1 and m.generation = $2
           order by m."index""#,
        set_id,
        generation as i32,
    )
    .fetch_all(conn)
    .await
}

/// Whether `subject` may use *every* member of the generation: no member lacks a
/// source the subject may `use`. The enqueue/dispatch set gate (decision #1: a
/// partially-inhabited generation is rejected). An empty generation is vacuously
/// usable (it simply matches no host at dispatch).
pub async fn generation_usable(
    conn: impl PgExecutor<'_>,
    subject: Uuid,
    set_id: Uuid,
    generation: u32,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"select not exists (
               select 1 from tml_switchboard.image_set_members m
               where m.set_id = $2 and m.generation = $3
                 and not tml_switchboard.image_source_usable($1, m.image_id)
           ) as "usable!""#,
        subject,
        set_id,
        generation as i32,
    )
    .fetch_one(conn)
    .await
}

/// Per-member usability of a generation for `viewer`, in member (`index`) order:
/// whether the viewer may use some source of each member, and whether *every*
/// subject holding a `use` grant on the set can (for a public set, the grantees
/// includes the `everyone` subject, folding the public case in). Feeds the API's
/// per-member "no source you can use" / "unusable by some grantee" indicators.
pub async fn generation_member_usability(
    conn: impl PgExecutor<'_>,
    viewer: Uuid,
    set_id: Uuid,
    generation: u32,
) -> Result<Vec<MemberUsability>, sqlx::Error> {
    sqlx::query_as!(
        MemberUsability,
        r#"select m.image_id,
                  tml_switchboard.image_source_usable($1, m.image_id) as "usable!",
                  not exists (
                      select 1
                      from tml_switchboard.image_set_grants ggr
                      where ggr.set_id = $2 and ggr.permission = 'use'
                        and not tml_switchboard.image_source_usable(ggr.subject_id, m.image_id)
                  ) as "usable_by_grantees!"
           from tml_switchboard.image_set_members m
           where m.set_id = $2 and m.generation = $3
           order by m."index""#,
        viewer,
        set_id,
        generation as i32,
    )
    .fetch_all(conn)
    .await
}

// -- image-set grants ---------------------------------------------------------

/// Grant `permission` ("use" / "manage") on a set to a subject; idempotent.
pub async fn grant_image_set(
    conn: impl PgExecutor<'_>,
    set_id: Uuid,
    subject_id: Uuid,
    permission: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"insert into tml_switchboard.image_set_grants
             (set_id, subject_id, permission)
           values ($1, $2, $3::text::tml_switchboard.image_set_permission)
           on conflict (set_id, subject_id, permission) do nothing"#,
        set_id,
        subject_id,
        permission,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Revoke a single `(subject, permission)` grant on a set. Returns whether a
/// row was removed.
pub async fn revoke_image_set(
    conn: impl PgExecutor<'_>,
    set_id: Uuid,
    subject_id: Uuid,
    permission: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"delete from tml_switchboard.image_set_grants
           where set_id = $1 and subject_id = $2
             and permission = $3::text::tml_switchboard.image_set_permission
           returning set_id"#,
        set_id,
        subject_id,
        permission,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// All grants on a set.
pub async fn list_image_set_grants(
    conn: impl PgExecutor<'_>,
    set_id: Uuid,
) -> Result<Vec<SetGrantRecord>, sqlx::Error> {
    sqlx::query_as!(
        SetGrantRecord,
        r#"select subject_id, permission::text as "permission!"
           from tml_switchboard.image_set_grants
           where set_id = $1
           order by granted_at, subject_id, permission"#,
        set_id,
    )
    .fetch_all(conn)
    .await
}
