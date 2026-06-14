//! Image-catalog REST routes: register images / image groups by digest and
//! list/inspect them (`doc/oci-image-migration-plan.md` §8.1).
//!
//! Registration pulls the manifest (or index) from the user's registry **by
//! digest**, validates it is a well-formed Treadmill artifact via
//! [`treadmill_rs::image::parse`], and records reference rows — never bytes.
//! Group registration additionally pulls and validates each member manifest and
//! denormalizes the members into `image_group_members` for the dispatch-time
//! matcher.

use axum::Json;
use axum::extract::{Path, State};
use http::StatusCode;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use oci_spec::image::{ImageIndex, ImageManifest};
use treadmill_rs::api::switchboard::images::{
    ImageGroupInfo, ImageGroupMember, ImageInfo, ImageLocation, RegisterImageGroupRequest,
    RegisterImageRequest,
};
use treadmill_rs::image::parse::{self, ParseError};
use treadmill_rs::image::{Digest, media_types};

use crate::registry::RegistryError;
use crate::serve::AppState;
use crate::sql::image;

/// Map any unexpected error to a 500, logging it.
fn internal(e: impl std::fmt::Display) -> StatusCode {
    tracing::error!("image route internal error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}

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
    Path(digest): Path<String>,
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

/// Ensure a group member's image is registered, returning its row id. The member
/// manifest is pulled (same repository as the index), validated, and recorded
/// with an `external` location if not already present.
async fn ensure_member_image(
    state: &AppState,
    owner: Uuid,
    registry: &str,
    repository: &str,
    digest: &Digest,
) -> Result<Uuid, StatusCode> {
    let digest_str = digest.encoded();
    if let Some(existing) = image::fetch_by_digest(state.pool(), &digest_str)
        .await
        .map_err(internal)?
    {
        image::upsert_location(state.pool(), existing.id, registry, repository, "external")
            .await
            .map_err(internal)?;
        return Ok(existing.id);
    }

    let bytes = pull_verified(state, registry, repository, digest).await?;
    let manifest: ImageManifest = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!("group member {digest} is not valid OCI JSON: {e}");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    let parsed = parse::parse_image(&manifest).map_err(invalid)?;
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
        None,
        &attrs,
    )
    .await
    .map_err(internal)?;
    image::upsert_location(&mut *tx, id, registry, repository, "external")
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(id)
}

/// `POST /image-groups`: register an image group by its OCI index digest.
pub async fn register_image_group(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<RegisterImageGroupRequest>,
) -> Result<(StatusCode, Json<ImageGroupInfo>), StatusCode> {
    let owner = subject.user_id();

    let bytes = pull_verified(&state, &req.registry, &req.repository, &req.index_digest).await?;
    let index: ImageIndex = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!("index {} is not valid OCI JSON: {e}", req.index_digest);
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    let members = parse::parse_group(&index).map_err(invalid)?;

    let index_str = req.index_digest.encoded();

    // First registrant owns the group (index_digest is unique).
    let existing = image::fetch_group_by_digest(state.pool(), &index_str)
        .await
        .map_err(internal)?;
    if let Some(existing) = &existing
        && !subject_reaches(&state, owner, existing.owner_subject).await?
    {
        return Err(StatusCode::CONFLICT);
    }

    // Validate + register every member before recording the group, so a bad
    // member rejects the whole registration.
    let mut resolved = Vec::with_capacity(members.len());
    for member in &members {
        let image_id = ensure_member_image(
            &state,
            owner,
            &req.registry,
            &req.repository,
            &member.digest,
        )
        .await?;
        resolved.push((image_id, member));
    }

    let group_id = match &existing {
        Some(g) => g.id,
        None => {
            let id = Uuid::now_v7();
            image::insert_group(state.pool(), id, &index_str, owner, req.label.as_deref())
                .await
                .map_err(internal)?;
            id
        }
    };

    // Rebuild the denormalized member set from the index, preserving index order
    // as `position` for deterministic matcher tie-breaks.
    image::clear_members(state.pool(), group_id)
        .await
        .map_err(internal)?;
    for (position, (image_id, member)) in resolved.iter().enumerate() {
        image::insert_member(
            state.pool(),
            group_id,
            *image_id,
            &member.required_host_tags,
            position as i32,
        )
        .await
        .map_err(internal)?;
    }

    let status = if existing.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let info = group_info(&state, group_id, &index_str).await?;
    Ok((status, Json(info)))
}

/// Assemble the API view of a group from its id, reading back the members.
async fn group_info(
    state: &AppState,
    group_id: Uuid,
    index_digest: &str,
) -> Result<ImageGroupInfo, StatusCode> {
    let group = image::fetch_group_by_digest(state.pool(), index_digest)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("group vanished immediately after insert"))?;
    let members = image::members_for_group(state.pool(), group_id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|m| {
            Ok(ImageGroupMember {
                manifest_digest: m.manifest_digest.parse().map_err(internal)?,
                required_host_tags: m.required_host_tags,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;
    Ok(ImageGroupInfo {
        id: group.id,
        index_digest: group.index_digest.parse().map_err(internal)?,
        owner_id: group.owner_subject,
        label: group.label,
        created_at: group.created_at,
        members,
    })
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
        out.push(group_info(&state, g.id, &g.index_digest).await?);
    }
    Ok(Json(out))
}

/// `GET /image-groups/{digest}`: inspect one group the caller can reach.
pub async fn get_image_group(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(digest): Path<String>,
) -> Result<Json<ImageGroupInfo>, StatusCode> {
    let group = image::fetch_group_by_digest(state.pool(), &digest)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !subject_reaches(&state, subject.user_id(), group.owner_subject).await? {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(
        group_info(&state, group.id, &group.index_digest).await?,
    ))
}
