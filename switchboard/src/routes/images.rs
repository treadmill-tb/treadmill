//! Image-catalog REST routes.
//!
//! Concrete images are identified by their OCI manifest digest and registered
//! implicitly when their first source is added: adding a source pulls the
//! manifest from the user's registry, validates it is a well-formed Treadmill
//! artifact via [`treadmill_rs::image::parse`], and records reference rows —
//! never bytes (`doc/oci-image-migration-plan.md` §8.1). The image row is a
//! non-owned cache of the manifest's projections; its database id never leaves
//! the API.
//!
//! Image *sets* are mutable, named, generationed switchboard entities: a set
//! is created empty, and its membership is replaced wholesale by appending
//! immutable generations whose members are pre-registered images referenced by
//! digest. No registry pull is involved.

use axum::Json;
use axum::extract::Path;
use axum::extract::{Query, State};
use http::StatusCode;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use oci_spec::image::ImageManifest;
use treadmill_rs::api::switchboard::images::{
    AddImageSourceRequest, CreateGenerationRequest, CreateImageSetRequest, GenerationMemberInfo,
    ImageInfo, ImageSetGenerationInfo, ImageSetGrantInfo, ImageSetGrantRequest, ImageSetInfo,
    ImageSetPermission, ImageSourceGrantInfo, ImageSourceGrantRequest, ImageSourceInfo,
    ImageSourcePermission,
};
use treadmill_rs::image::parse::{self, ParseError};
use treadmill_rs::image::{Digest, media_types};

use crate::audit::feed::{AuditFeedQuery, AuditFeedResponse, fetch_events_for_entity};
use crate::audit::model::{ImageSet as AuditImageSet, Subject as AuditSubject};
use crate::audit::{self, events};
use crate::auth::engine::{self, ImageSetPermission as Perm, ImageSourcePermission as SourcePerm};
use crate::http_error::internal;
use crate::registry::RegistryError;
use crate::routes::params::{
    DigestPath, GenerationPath, GrantPath, IdPath, SourceGrantPath, SourcePath,
};
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

/// Assemble the API view of an image from its record plus its sources, surfacing
/// `viewer`'s permissions on each source.
async fn image_info(
    state: &AppState,
    rec: image::ImageRecord,
    viewer: Uuid,
) -> Result<ImageInfo, StatusCode> {
    let source_recs = image::sources_for_image(state.pool(), rec.id)
        .await
        .map_err(internal)?;
    let mut sources = Vec::with_capacity(source_recs.len());
    for s in source_recs {
        let permissions = engine::image_source_permissions(state.pool(), viewer, s.id)
            .await
            .map_err(internal)?
            .into_iter()
            .map(source_perm_to_api)
            .collect();
        sources.push(ImageSourceInfo {
            id: s.id,
            registry: s.registry,
            repository: s.repository,
            status: s.status,
            owner_id: s.owner_subject,
            permissions,
        });
    }
    Ok(ImageInfo {
        manifest_digest: rec.manifest_digest.parse().map_err(internal)?,
        artifact_type: rec.artifact_type,
        title: rec.title,
        created_at: rec.created_at,
        sources,
    })
}

/// Map the engine's source permission to the API enum.
fn source_perm_to_api(p: SourcePerm) -> ImageSourcePermission {
    match p {
        SourcePerm::Use => ImageSourcePermission::Use,
        SourcePerm::Manage => ImageSourcePermission::Manage,
    }
}

/// `GET /images`: list images the caller may use (has a reachable source).
pub async fn list_images(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<Vec<ImageInfo>>, StatusCode> {
    let viewer = subject.user_id();
    let records = image::list_usable_images(state.pool(), viewer)
        .await
        .map_err(internal)?;
    let mut out = Vec::with_capacity(records.len());
    for rec in records {
        out.push(image_info(&state, rec, viewer).await?);
    }
    Ok(Json(out))
}

/// `GET /images/{digest}`: inspect one image the caller can reach (has a source
/// it may use). 404 if none, so existence is not leaked.
pub async fn get_image(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(DigestPath { digest }): Path<DigestPath>,
) -> Result<Json<ImageInfo>, StatusCode> {
    let viewer = subject.user_id();
    let rec = image::fetch_by_digest(state.pool(), &digest)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    // Don't leak existence of images the caller has no usable source for.
    let usable = image::image_source_usable(state.pool(), viewer, rec.id)
        .await
        .map_err(internal)?;
    if !usable {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(image_info(&state, rec, viewer).await?))
}

/// Assemble the API view of a set from its record, reading the latest
/// generation number.
async fn set_info(state: &AppState, set: image::SetRecord) -> Result<ImageSetInfo, StatusCode> {
    let latest_generation = image::latest_generation(state.pool(), set.id)
        .await
        .map_err(internal)?;
    Ok(ImageSetInfo {
        id: set.id,
        name: set.name,
        label: set.label,
        owner_id: set.owner_subject,
        created_at: set.created_at,
        latest_generation,
    })
}

/// `POST /image-sets`: create an empty, named image set. The caller owns it.
pub async fn create_image_set(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<CreateImageSetRequest>,
) -> Result<(StatusCode, Json<ImageSetInfo>), StatusCode> {
    let owner = subject.user_id();

    // Names are globally unique; surface a clash as a 409 rather than a 500.
    if image::fetch_set_by_name(state.pool(), &req.name)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(StatusCode::CONFLICT);
    }

    let id = Uuid::now_v7();
    let mut tx = state.pool().begin().await.map_err(internal)?;
    image::create_set(&mut *tx, id, &req.name, owner, req.label.as_deref())
        .await
        .map_err(internal)?;
    audit::emit(
        &mut tx,
        &events::ImageSetCreated {
            actor: AuditSubject(owner),
            owner: AuditSubject(owner),
            set: AuditImageSet(id),
            name: req.name.clone(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    let set = image::fetch_set_by_id(state.pool(), id)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("set vanished immediately after insert"))?;
    Ok((StatusCode::CREATED, Json(set_info(&state, set).await?)))
}

/// `GET /image-sets`: list sets the caller owns (directly or via a group).
pub async fn list_image_sets(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<Vec<ImageSetInfo>>, StatusCode> {
    let sets = image::list_owned_sets(state.pool(), subject.user_id())
        .await
        .map_err(internal)?;
    let mut out = Vec::with_capacity(sets.len());
    for g in sets {
        out.push(set_info(&state, g).await?);
    }
    Ok(Json(out))
}

/// Load a set, returning 404 unless the caller may at least `use` it (owner,
/// `use`/`manage` grant, or admin). Don't leak existence to others.
async fn visible_set(
    state: &AppState,
    subject: Uuid,
    set_id: Uuid,
) -> Result<image::SetRecord, StatusCode> {
    let set = image::fetch_set_by_id(state.pool(), set_id)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let visible = engine::can_access_image_set(state.pool(), subject, set_id, Perm::Use)
        .await
        .map_err(internal)?;
    if !visible {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(set)
}

/// Require `manage` on a set (owner or admin implicitly), 404 if the caller
/// cannot even see it, 403 if they can see it but lack `manage`.
async fn require_manage(
    state: &AppState,
    subject: Uuid,
    set_id: Uuid,
) -> Result<image::SetRecord, StatusCode> {
    let set = visible_set(state, subject, set_id).await?;
    let manage = engine::can_access_image_set(state.pool(), subject, set_id, Perm::Manage)
        .await
        .map_err(internal)?;
    if !manage {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(set)
}

/// `GET /image-sets/{id}`: inspect one set the caller can reach.
pub async fn get_image_set(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: set_id }): Path<IdPath>,
) -> Result<Json<ImageSetInfo>, StatusCode> {
    let set = visible_set(&state, subject.user_id(), set_id).await?;
    Ok(Json(set_info(&state, set).await?))
}

/// Assemble the API view of a generation, reading back its members in `index`
/// order.
async fn generation_info(
    state: &AppState,
    viewer: Uuid,
    set_id: Uuid,
    generation: u32,
) -> Result<ImageSetGenerationInfo, StatusCode> {
    let gen_row = image::fetch_generation(state.pool(), set_id, generation)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    // Per-member usability (viewer + all set `use`-grantees), keyed by image id.
    // A member's set grant is necessary but not sufficient: it may still lack a
    // source the viewer (or some grantee) can reach.
    let usability: std::collections::HashMap<Uuid, image::MemberUsability> =
        image::generation_member_usability(state.pool(), viewer, set_id, generation)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|u| (u.image_id, u))
            .collect();
    let members = image::members_for_generation(state.pool(), set_id, generation)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|m| {
            let (usable, usable_by_grantees) = usability
                .get(&m.image_id)
                .map(|u| (u.usable, u.usable_by_grantees))
                .unwrap_or((false, false));
            Ok(GenerationMemberInfo {
                manifest_digest: m.manifest_digest.parse().map_err(internal)?,
                required_host_tags: m.required_host_tags,
                index: m.index as u32,
                usable,
                usable_by_grantees,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;
    Ok(ImageSetGenerationInfo {
        set_id,
        generation,
        created_at: gen_row.created_at,
        created_by: gen_row.created_by,
        members,
    })
}

/// `POST /image-sets/{id}/generations`: append a full-replacement generation.
/// Requires `manage`. Every member digest must already be registered;
/// `required_host_tags` come from the payload, `index` from order.
pub async fn create_generation(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: set_id }): Path<IdPath>,
    Json(req): Json<CreateGenerationRequest>,
) -> Result<(StatusCode, Json<ImageSetGenerationInfo>), StatusCode> {
    require_manage(&state, subject.user_id(), set_id).await?;

    // Resolve every member digest to a registered image up front, and build the
    // `(image_id, tags, index)` rows in array order.
    let mut members = Vec::with_capacity(req.members.len());
    for (index, m) in req.members.iter().enumerate() {
        let rec = image::fetch_by_digest(state.pool(), &m.manifest_digest.encoded())
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                tracing::warn!(
                    "create_generation references unregistered image {}",
                    m.manifest_digest
                );
                StatusCode::UNPROCESSABLE_ENTITY
            })?;
        members.push((rec.id, m.required_host_tags.clone(), index as i32));
    }

    let mut tx = state.pool().begin().await.map_err(internal)?;
    let generation = image::create_generation(&mut tx, set_id, subject.user_id(), &members)
        .await
        .map_err(internal)?;
    audit::emit(
        &mut tx,
        &events::ImageSetGenerationCreated {
            actor: AuditSubject(subject.user_id()),
            set: AuditImageSet(set_id),
            generation: i64::from(generation),
            member_count: members.len() as i64,
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    let info = generation_info(&state, subject.user_id(), set_id, generation).await?;
    Ok((StatusCode::CREATED, Json(info)))
}

/// `GET /image-sets/{id}/generations/{n}`: inspect one generation.
pub async fn get_generation(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(GenerationPath {
        id: set_id,
        n: generation,
    }): Path<GenerationPath>,
) -> Result<Json<ImageSetGenerationInfo>, StatusCode> {
    visible_set(&state, subject.user_id(), set_id).await?;
    Ok(Json(
        generation_info(&state, subject.user_id(), set_id, generation).await?,
    ))
}

/// `POST /image-sets/{id}/grants`: grant `use`/`manage` to a subject. Requires
/// `manage`.
pub async fn grant_image_set(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: set_id }): Path<IdPath>,
    Json(req): Json<ImageSetGrantRequest>,
) -> Result<StatusCode, StatusCode> {
    require_manage(&state, subject.user_id(), set_id).await?;
    let permission = Perm::from(req.permission);
    let mut tx = state.pool().begin().await.map_err(internal)?;
    image::grant_image_set(&mut *tx, set_id, req.subject_id, permission.as_str())
        .await
        .map_err(internal)?;
    audit::emit(
        &mut tx,
        &events::ImageSetGrantCreated {
            actor: AuditSubject(subject.user_id()),
            set: AuditImageSet(set_id),
            grantee: AuditSubject(req.subject_id),
            permission: permission.as_str().to_string(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /image-sets/{id}/grants`: list a set's grants. Requires `manage`.
pub async fn list_image_set_grants(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: set_id }): Path<IdPath>,
) -> Result<Json<Vec<ImageSetGrantInfo>>, StatusCode> {
    require_manage(&state, subject.user_id(), set_id).await?;
    let grants = image::list_image_set_grants(state.pool(), set_id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|g| {
            let permission = match g.permission.as_str() {
                "use" => ImageSetPermission::Use,
                "manage" => ImageSetPermission::Manage,
                other => {
                    return Err(internal(format!("unknown image-set permission {other:?}")));
                }
            };
            Ok(ImageSetGrantInfo {
                subject_id: g.subject_id,
                permission,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;
    Ok(Json(grants))
}

/// `DELETE /image-sets/{id}/grants/{subject_id}/{permission}`: revoke a grant.
/// Requires `manage`.
pub async fn revoke_image_set_grant(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(GrantPath {
        id: set_id,
        subject_id: target,
        permission,
    }): Path<GrantPath>,
) -> Result<StatusCode, StatusCode> {
    require_manage(&state, subject.user_id(), set_id).await?;
    // Reject an unknown permission word with a 400 rather than silently no-op.
    if permission != "use" && permission != "manage" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut tx = state.pool().begin().await.map_err(internal)?;
    let removed = image::revoke_image_set(&mut *tx, set_id, target, &permission)
        .await
        .map_err(internal)?;
    // Only a grant that actually existed is an auditable change.
    if removed {
        audit::emit(
            &mut tx,
            &events::ImageSetGrantRevoked {
                actor: AuditSubject(subject.user_id()),
                set: AuditImageSet(set_id),
                grantee: AuditSubject(target),
                permission: permission.clone(),
            },
        )
        .await
        .map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// `GET /image-sets/{id}/events`: the set's audit feed (grants, generations,
/// creation). Gated on `manage` — the events carry the `manage` view policy, so
/// a viewer who only holds `use` would see an empty feed; requiring `manage`
/// makes that explicit (404 if the set is not even visible, 403 if visible but
/// unmanaged). "Public" is a grant to the `everyone` subject, so making a set
/// public/private appears here as an ordinary grant/revoke event.
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: set_id }): Path<IdPath>,
    Query(query): Query<AuditFeedQuery>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    require_manage(&state, subject.user_id(), set_id).await?;
    fetch_events_for_entity(&state, &subject, "image_set", set_id, &query)
        .await
        .map(Json)
}

// -- image sources --------------------------------------------------------------

/// `POST /images/{digest}/sources`: add a registry source for an image,
/// registering the image on first sight. An image is a non-owned manifest
/// identity created (or reused) implicitly when a source naming its digest is
/// added; the caller owns the *source* it adds.
///
/// The manifest is pulled from the submitted source, verified against the path
/// digest, and validated as a Treadmill artifact before anything is recorded,
/// so a dispatch never picks an unusable source. An unknown digest inserts the
/// image row (caching the manifest's projections: title, head, layer count,
/// version, description) plus the source (201); a known digest idempotently
/// gains the caller-owned source (200).
pub async fn add_image_source(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(DigestPath { digest }): Path<DigestPath>,
    Json(req): Json<AddImageSourceRequest>,
) -> Result<(StatusCode, Json<ImageInfo>), StatusCode> {
    let owner = subject.user_id();
    let parsed_digest: Digest = digest.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    let bytes = pull_verified(&state, &req.registry, &req.repository, &parsed_digest).await?;
    let manifest: ImageManifest = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!("manifest {parsed_digest} is not valid OCI JSON: {e}");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    let parsed = parse::parse_image(&manifest).map_err(invalid)?;

    let digest_str = parsed_digest.encoded();

    // A known digest is a shared, non-owned identity: any caller may add a source
    // it owns (idempotent on `(image_id, registry, repository)`).
    if let Some(existing) = image::fetch_by_digest(state.pool(), &digest_str)
        .await
        .map_err(internal)?
    {
        image::insert_source(
            state.pool(),
            Uuid::now_v7(),
            existing.id,
            &req.registry,
            &req.repository,
            "external",
            Some(owner),
        )
        .await
        .map_err(internal)?;
        return Ok((
            StatusCode::OK,
            Json(image_info(&state, existing, owner).await?),
        ));
    }

    let attrs = serde_json::json!({
        "head": parsed.head.encoded(),
        "layers": parsed.layers.len(),
        "version": parsed.version,
        "description": parsed.description,
    });

    let id = Uuid::now_v7();
    let mut tx = state.pool().begin().await.map_err(internal)?;
    image::insert(
        &mut *tx,
        id,
        &digest_str,
        media_types::IMAGE_ARTIFACT_TYPE,
        parsed.title.as_deref(),
        &attrs,
    )
    .await
    .map_err(internal)?;
    image::insert_source(
        &mut *tx,
        Uuid::now_v7(),
        id,
        &req.registry,
        &req.repository,
        "external",
        Some(owner),
    )
    .await
    .map_err(internal)?;
    audit::emit(
        &mut tx,
        &events::ImageRegistered {
            actor: AuditSubject(owner),
            owner: AuditSubject(owner),
            image_id: id,
            manifest_digest: digest_str.clone(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    let rec = image::fetch_by_digest(state.pool(), &digest_str)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("image vanished immediately after insert"))?;
    Ok((
        StatusCode::CREATED,
        Json(image_info(&state, rec, owner).await?),
    ))
}

/// Load a source that belongs to the image identified by `digest`, requiring the
/// caller hold `manage` on it (owner or admin implicitly). 404 if the image or
/// source is unknown or the source belongs to a different image; 403 if the
/// caller may not manage it.
async fn require_source_manage(
    state: &AppState,
    subject: Uuid,
    digest: &str,
    source_id: Uuid,
) -> Result<image::SourceRecord, StatusCode> {
    let img = image::fetch_by_digest(state.pool(), digest)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let source = image::fetch_source(state.pool(), source_id)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if source.image_id != img.id {
        return Err(StatusCode::NOT_FOUND);
    }
    let manage =
        engine::can_access_image_source(state.pool(), subject, source_id, SourcePerm::Manage)
            .await
            .map_err(internal)?;
    if !manage {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(source)
}

/// `DELETE /images/{digest}/sources/{source_id}`: delete a source. Requires
/// `manage`. Sources are always deletable by their owner/admins, even when a
/// generation references the image.
pub async fn delete_image_source(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(SourcePath { digest, source_id }): Path<SourcePath>,
) -> Result<StatusCode, StatusCode> {
    require_source_manage(&state, subject.user_id(), &digest, source_id).await?;
    let removed = image::delete_source(state.pool(), source_id)
        .await
        .map_err(internal)?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// `POST /images/{digest}/sources/{source_id}/grants`: grant `use`/`manage` on a
/// source to a subject. Requires `manage`. Granting the `everyone` subject `use`
/// makes the source public.
pub async fn grant_image_source(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(SourcePath { digest, source_id }): Path<SourcePath>,
    Json(req): Json<ImageSourceGrantRequest>,
) -> Result<StatusCode, StatusCode> {
    require_source_manage(&state, subject.user_id(), &digest, source_id).await?;
    let permission = SourcePerm::from(req.permission);
    image::grant_image_source(state.pool(), source_id, req.subject_id, permission.as_str())
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /images/{digest}/sources/{source_id}/grants`: list a source's grants.
/// Requires `manage`.
pub async fn list_image_source_grants(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(SourcePath { digest, source_id }): Path<SourcePath>,
) -> Result<Json<Vec<ImageSourceGrantInfo>>, StatusCode> {
    require_source_manage(&state, subject.user_id(), &digest, source_id).await?;
    let grants = image::list_image_source_grants(state.pool(), source_id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|g| {
            let permission = match g.permission.as_str() {
                "use" => ImageSourcePermission::Use,
                "manage" => ImageSourcePermission::Manage,
                other => {
                    return Err(internal(format!(
                        "unknown image-source permission {other:?}"
                    )));
                }
            };
            Ok(ImageSourceGrantInfo {
                subject_id: g.subject_id,
                permission,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;
    Ok(Json(grants))
}

/// `DELETE /images/{digest}/sources/{source_id}/grants/{subject_id}/{permission}`:
/// revoke a grant. Requires `manage`.
pub async fn revoke_image_source_grant(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(SourceGrantPath {
        digest,
        source_id,
        subject_id: target,
        permission,
    }): Path<SourceGrantPath>,
) -> Result<StatusCode, StatusCode> {
    require_source_manage(&state, subject.user_id(), &digest, source_id).await?;
    // Reject an unknown permission word with a 400 rather than silently no-op.
    if permission != "use" && permission != "manage" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let removed = image::revoke_image_source(state.pool(), source_id, target, &permission)
        .await
        .map_err(internal)?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}
