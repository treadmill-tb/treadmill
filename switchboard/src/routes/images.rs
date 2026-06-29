//! Image-catalog REST routes.
//!
//! Concrete images are registered by digest: registration pulls the manifest
//! from the user's registry, validates it is a well-formed Treadmill artifact via
//! [`treadmill_rs::image::parse`], and records reference rows — never bytes
//! (`doc/oci-image-migration-plan.md` §8.1).
//!
//! Image *groups* are mutable, named, generationed switchboard entities: a group
//! is created empty, and its membership is replaced wholesale by appending
//! immutable generations whose members are pre-registered images referenced by id.
//! No registry pull is involved.

use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use http::StatusCode;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use oci_spec::image::ImageManifest;
use treadmill_rs::api::switchboard::images::{
    CreateGenerationRequest, CreateImageGroupRequest, GenerationMemberInfo,
    ImageGroupGenerationInfo, ImageGroupGrantInfo, ImageGroupGrantRequest, ImageGroupInfo,
    ImageGroupPermission, ImageInfo, ImageLocation, RegisterImageRequest,
    SetImageGroupPublicRequest,
};
use treadmill_rs::image::parse::{self, ParseError};
use treadmill_rs::image::{Digest, media_types};

use crate::auth::engine::{self, ImageGroupPermission as Perm};
use crate::http_error::internal;
use crate::registry::RegistryError;
use crate::routes::params::{DigestPath, GenerationPath, GrantPath, IdPath};
use crate::serve::AppState;
use crate::sql::image;

/// A pull failure is the caller's problem (bad registry/repo/digest): 502 so it
/// is distinguishable from a switchboard fault, with the cause logged.
fn pull_failed(e: RegistryError) -> StatusCode {
    tracing::warn!("image registration pull failed: {e}");
    StatusCode::BAD_GATEWAY
}

/// A manifest that does not validate as a Treadmill artifact is a 422.
fn invalid(e: ParseError) -> StatusCode {
    tracing::warn!("image registration validation failed: {e}");
    StatusCode::UNPROCESSABLE_ENTITY
}

/// Whether `subject` can reach `target` (is it, or transitively contains it).
async fn subject_reaches(
    state: &AppState,
    subject: Uuid,
    target: Option<Uuid>,
) -> Result<bool, StatusCode> {
    let Some(target) = target else {
        return Ok(false);
    };
    sqlx::query_scalar!(
        "select exists(select 1 from tml_switchboard.principals($1) p where p.id = $2) as \"ok!\"",
        subject,
        target,
    )
    .fetch_one(state.pool())
    .await
    .map_err(internal)
}

/// Pull a manifest/index document by digest and verify it hashes to that digest.
async fn pull_verified(
    state: &AppState,
    registry: &str,
    repository: &str,
    digest: &Digest,
) -> Result<Vec<u8>, StatusCode> {
    let bytes = state
        .registry()
        .fetch_manifest(registry, repository, digest)
        .await
        .map_err(pull_failed)?;

    let got = Sha256::digest(&bytes);
    if got.as_slice() != digest.as_bytes() {
        tracing::warn!(
            "registry served a document whose digest does not match the request {digest}"
        );
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    Ok(bytes)
}

/// Assemble the API view of an image from its record plus its locations.
async fn image_info(state: &AppState, rec: image::ImageRecord) -> Result<ImageInfo, StatusCode> {
    let locations = image::locations_for_image(state.pool(), rec.id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|l| ImageLocation {
            registry: l.registry,
            repository: l.repository,
            status: l.status,
        })
        .collect();
    Ok(ImageInfo {
        id: rec.id,
        manifest_digest: rec.manifest_digest.parse().map_err(internal)?,
        artifact_type: rec.artifact_type,
        owner_id: rec.owner_subject,
        label: rec.label,
        created_at: rec.created_at,
        locations,
    })
}

/// `POST /images`: register a concrete image by digest.
pub async fn register_image(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<RegisterImageRequest>,
) -> Result<(StatusCode, Json<ImageInfo>), StatusCode> {
    let owner = subject.user_id();

    // Pull and validate the manifest before recording anything.
    let bytes = pull_verified(&state, &req.registry, &req.repository, &req.manifest_digest).await?;
    let manifest: ImageManifest = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!(
            "manifest {} is not valid OCI JSON: {e}",
            req.manifest_digest
        );
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    let parsed = parse::parse_image(&manifest).map_err(invalid)?;

    let digest_str = req.manifest_digest.encoded();

    // A digest is owned by its first registrant (manifest_digest is unique).
    if let Some(existing) = image::fetch_by_digest(state.pool(), &digest_str)
        .await
        .map_err(internal)?
    {
        if !subject_reaches(&state, owner, existing.owner_subject).await? {
            return Err(StatusCode::CONFLICT);
        }
        // Re-registration by the owner: idempotently add the location (e.g. a
        // new mirror), leaving the digest and existing references unchanged.
        image::upsert_location(
            state.pool(),
            existing.id,
            &req.registry,
            &req.repository,
            "external",
        )
        .await
        .map_err(internal)?;
        return Ok((StatusCode::OK, Json(image_info(&state, existing).await?)));
    }

    let attrs = serde_json::json!({
        "title": parsed.title,
        "head": parsed.head.encoded(),
        "layers": parsed.layers.len(),
    });

    let id = Uuid::now_v7();
    let mut tx = state.pool().begin().await.map_err(internal)?;
    image::insert(
        &mut *tx,
        id,
        &digest_str,
        media_types::IMAGE_ARTIFACT_TYPE,
        owner,
        req.label.as_deref(),
        &attrs,
    )
    .await
    .map_err(internal)?;
    image::upsert_location(&mut *tx, id, &req.registry, &req.repository, "external")
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    let rec = image::fetch_by_digest(state.pool(), &digest_str)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("image vanished immediately after insert"))?;
    Ok((StatusCode::CREATED, Json(image_info(&state, rec).await?)))
}

/// `GET /images`: list images the caller owns (directly or via a group).
pub async fn list_images(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<Vec<ImageInfo>>, StatusCode> {
    let records = image::list_owned(state.pool(), subject.user_id())
        .await
        .map_err(internal)?;
    let mut out = Vec::with_capacity(records.len());
    for rec in records {
        out.push(image_info(&state, rec).await?);
    }
    Ok(Json(out))
}

/// `GET /images/{digest}`: inspect one image the caller can reach.
pub async fn get_image(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(DigestPath { digest }): Path<DigestPath>,
) -> Result<Json<ImageInfo>, StatusCode> {
    let rec = image::fetch_by_digest(state.pool(), &digest)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    // Don't leak existence of images the caller cannot reach.
    if !subject_reaches(&state, subject.user_id(), rec.owner_subject).await? {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(image_info(&state, rec).await?))
}

/// Assemble the API view of a group from its record, reading the latest
/// generation number.
async fn group_info(
    state: &AppState,
    group: image::GroupRecord,
) -> Result<ImageGroupInfo, StatusCode> {
    let latest_generation = image::latest_generation(state.pool(), group.id)
        .await
        .map_err(internal)?;
    Ok(ImageGroupInfo {
        id: group.id,
        name: group.name,
        label: group.label,
        owner_id: group.owner_subject,
        public: group.public,
        created_at: group.created_at,
        latest_generation,
    })
}

/// `POST /image-groups`: create an empty, named image group. The caller owns it.
pub async fn create_image_group(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<CreateImageGroupRequest>,
) -> Result<(StatusCode, Json<ImageGroupInfo>), StatusCode> {
    let owner = subject.user_id();

    // Names are globally unique; surface a clash as a 409 rather than a 500.
    if image::fetch_group_by_name(state.pool(), &req.name)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(StatusCode::CONFLICT);
    }

    let id = Uuid::now_v7();
    image::create_group(
        state.pool(),
        id,
        &req.name,
        owner,
        req.label.as_deref(),
        req.public,
    )
    .await
    .map_err(internal)?;

    let group = image::fetch_group_by_id(state.pool(), id)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("group vanished immediately after insert"))?;
    Ok((StatusCode::CREATED, Json(group_info(&state, group).await?)))
}

/// `GET /image-groups`: list groups the caller owns (directly or via a group).
pub async fn list_image_groups(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<Vec<ImageGroupInfo>>, StatusCode> {
    let groups = image::list_owned_groups(state.pool(), subject.user_id())
        .await
        .map_err(internal)?;
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        out.push(group_info(&state, g).await?);
    }
    Ok(Json(out))
}

/// Load a group, returning 404 unless the caller may at least `use` it (owner,
/// `use`/`manage` grant, or admin). Don't leak existence to others.
async fn visible_group(
    state: &AppState,
    subject: Uuid,
    group_id: Uuid,
) -> Result<image::GroupRecord, StatusCode> {
    let group = image::fetch_group_by_id(state.pool(), group_id)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let visible = engine::can_access_image_group(state.pool(), subject, group_id, Perm::Use)
        .await
        .map_err(internal)?;
    if !visible {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(group)
}

/// Require `manage` on a group (owner or admin implicitly), 404 if the caller
/// cannot even see it, 403 if they can see it but lack `manage`.
async fn require_manage(
    state: &AppState,
    subject: Uuid,
    group_id: Uuid,
) -> Result<image::GroupRecord, StatusCode> {
    let group = visible_group(state, subject, group_id).await?;
    let manage = engine::can_access_image_group(state.pool(), subject, group_id, Perm::Manage)
        .await
        .map_err(internal)?;
    if !manage {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(group)
}

/// `GET /image-groups/{id}`: inspect one group the caller can reach.
pub async fn get_image_group(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: group_id }): Path<IdPath>,
) -> Result<Json<ImageGroupInfo>, StatusCode> {
    let group = visible_group(&state, subject.user_id(), group_id).await?;
    Ok(Json(group_info(&state, group).await?))
}

/// Assemble the API view of a generation, reading back its members in `index`
/// order.
async fn generation_info(
    state: &AppState,
    group_id: Uuid,
    generation: u32,
) -> Result<ImageGroupGenerationInfo, StatusCode> {
    let gen_row = image::fetch_generation(state.pool(), group_id, generation)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let members = image::members_for_generation(state.pool(), group_id, generation)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|m| {
            Ok(GenerationMemberInfo {
                image_id: m.image_id,
                manifest_digest: m.manifest_digest.parse().map_err(internal)?,
                required_host_tags: m.required_host_tags,
                index: m.index as u32,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;
    Ok(ImageGroupGenerationInfo {
        group_id,
        generation,
        created_at: gen_row.created_at,
        created_by: gen_row.created_by,
        members,
    })
}

/// `POST /image-groups/{id}/generations`: append a full-replacement generation.
/// Requires `manage`. Every `image_id` must already be registered (the FK also
/// enforces); `required_host_tags` come from the payload, `index` from order.
pub async fn create_generation(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: group_id }): Path<IdPath>,
    Json(req): Json<CreateGenerationRequest>,
) -> Result<(StatusCode, Json<ImageGroupGenerationInfo>), StatusCode> {
    require_manage(&state, subject.user_id(), group_id).await?;

    // Validate every member image exists up front (clearer than relying on the
    // FK violation), and build the `(image_id, tags, index)` rows in array order.
    let mut members = Vec::with_capacity(req.members.len());
    for (index, m) in req.members.iter().enumerate() {
        if image::fetch_by_id(state.pool(), m.image_id)
            .await
            .map_err(internal)?
            .is_none()
        {
            tracing::warn!(
                "create_generation references unregistered image {}",
                m.image_id
            );
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        members.push((m.image_id, m.required_host_tags.clone(), index as i32));
    }

    let mut tx = state.pool().begin().await.map_err(internal)?;
    let generation = image::create_generation(&mut tx, group_id, subject.user_id(), &members)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    let info = generation_info(&state, group_id, generation).await?;
    Ok((StatusCode::CREATED, Json(info)))
}

/// `GET /image-groups/{id}/generations/{n}`: inspect one generation.
pub async fn get_generation(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(GenerationPath {
        id: group_id,
        n: generation,
    }): Path<GenerationPath>,
) -> Result<Json<ImageGroupGenerationInfo>, StatusCode> {
    visible_group(&state, subject.user_id(), group_id).await?;
    Ok(Json(generation_info(&state, group_id, generation).await?))
}

/// `POST /image-groups/{id}/grants`: grant `use`/`manage` to a subject. Requires
/// `manage`.
pub async fn grant_image_group(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: group_id }): Path<IdPath>,
    Json(req): Json<ImageGroupGrantRequest>,
) -> Result<StatusCode, StatusCode> {
    require_manage(&state, subject.user_id(), group_id).await?;
    image::grant_image_group(
        state.pool(),
        group_id,
        req.subject_id,
        Perm::from(req.permission).as_str(),
    )
    .await
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /image-groups/{id}/grants`: list a group's grants. Requires `manage`.
pub async fn list_image_group_grants(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: group_id }): Path<IdPath>,
) -> Result<Json<Vec<ImageGroupGrantInfo>>, StatusCode> {
    require_manage(&state, subject.user_id(), group_id).await?;
    let grants = image::list_image_group_grants(state.pool(), group_id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|g| {
            let permission = match g.permission.as_str() {
                "use" => ImageGroupPermission::Use,
                "manage" => ImageGroupPermission::Manage,
                other => {
                    return Err(internal(format!(
                        "unknown image-group permission {other:?}"
                    )));
                }
            };
            Ok(ImageGroupGrantInfo {
                subject_id: g.subject_id,
                permission,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;
    Ok(Json(grants))
}

/// `DELETE /image-groups/{id}/grants/{subject_id}/{permission}`: revoke a grant.
/// Requires `manage`.
pub async fn revoke_image_group_grant(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(GrantPath {
        id: group_id,
        subject_id: target,
        permission,
    }): Path<GrantPath>,
) -> Result<StatusCode, StatusCode> {
    require_manage(&state, subject.user_id(), group_id).await?;
    // Reject an unknown permission word with a 400 rather than silently no-op.
    if permission != "use" && permission != "manage" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let removed = image::revoke_image_group(state.pool(), group_id, target, &permission)
        .await
        .map_err(internal)?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// `PUT /image-groups/{id}/public`: declare a group public (or private again).
/// This is part of the authorization surface — `public` is an implicit `use`
/// grant to *every* subject — so it sits alongside the per-subject grant routes
/// and is gated on `manage` like them, not treated as descriptive metadata.
pub async fn set_image_group_public(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: group_id }): Path<IdPath>,
    Json(req): Json<SetImageGroupPublicRequest>,
) -> Result<Json<ImageGroupInfo>, StatusCode> {
    require_manage(&state, subject.user_id(), group_id).await?;
    image::set_group_public(state.pool(), group_id, req.public)
        .await
        .map_err(internal)?;
    let group = image::fetch_group_by_id(state.pool(), group_id)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(group_info(&state, group).await?))
}
