use axum::extract::{Path, State};
use axum::Json;
use http::StatusCode;
use uuid::Uuid;
use schemars::JsonSchema;

use crate::audit::registry::{ViewerCtx, render};
use crate::auth::Subject;
use crate::auth::engine;
use crate::serve::AppState;
use serde::Serialize;

#[derive(Serialize, JsonSchema)]
pub struct AuditFeedResponse {
    pub events: Vec<RenderedAuditRow>,
}

#[derive(Serialize, JsonSchema)]
pub struct RenderedAuditRow {
    pub event_id: Uuid,
    pub event_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub actor_id: Uuid,
    pub correlation_id: Option<Uuid>,
    pub message: String,
}

pub async fn list_events(
    State(state): State<AppState>,
    subject: Subject,
    Path((entity_kind, entity_id)): Path<(String, Uuid)>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    let valid_kinds = ["job", "host", "subject"];
    if !valid_kinds.contains(&entity_kind.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let viewer_id = subject.user_id();
    let is_admin = engine::is_admin(state.pool(), viewer_id)
        .await
        .map_err(|e| {
            tracing::error!("failed to check admin status: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut allowed_policies: Vec<String> = Vec::new();

    if !is_admin {
        match entity_kind.as_str() {
            "host" => {
                let perms = engine::host_permissions(state.pool(), viewer_id, entity_id)
                    .await
                    .map_err(|e| {
                        tracing::error!("failed to compute host permissions: {e}");
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
                allowed_policies.extend(perms.into_iter().map(|p| p.as_str().to_string()));
            }
            "job" => {
                let perms = engine::job_permissions(state.pool(), viewer_id, entity_id)
                    .await
                    .map_err(|e| {
                        tracing::error!("failed to compute job permissions: {e}");
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
                allowed_policies.extend(perms.into_iter().map(|p| p.as_str().to_string()));
            }
            "subject" => {}
            _ => unreachable!(),
        }

        // If not admin and no allowed policies, they can't see anything.
        if allowed_policies.is_empty() {
            // They don't have access to this entity, or it's a subject and they are not admin.
            // Return 403 Forbidden to match standard access denied, or just an empty list.
            // Plan says: "Caller's access to X is already proven by the route's auth extractor."
            // But we don't have a specific auth extractor per-route here, we just use the global feed endpoint.
            // If they have no permissions, they just see an empty list. Returning 403 is better if they have *no* access.
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Fetch the events!
    let rows = sqlx::query!(
        r#"
        select e.event_id, e.event_type, e.payload, e.actor_id, e.correlation_id, e.created_at
        from tml_switchboard.audit_events e
        join tml_switchboard.audit_event_relations r on e.event_id = r.event_id
        where r.entity_kind = $1::tml_switchboard.audit_entity_kind
          and r.entity_id = $2
          and ($3::boolean or r.view_policy = any($4::text[]))
        order by e.created_at desc
        limit 100
        "#,
        entity_kind.as_str() as _,
        entity_id,
        is_admin,
        &allowed_policies,
    )
    .fetch_all(state.pool())
    .await
    .map_err(|e| {
        tracing::error!("failed to fetch audit events: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let viewer_ctx = ViewerCtx {
        viewer_id,
        global_authority: is_admin,
    };

    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let rendered = render(&row.event_type, &row.payload, &viewer_ctx);
        match rendered {
            Ok(r) => {
                events.push(RenderedAuditRow {
                    event_id: row.event_id,
                    event_type: row.event_type,
                    created_at: row.created_at,
                    actor_id: row.actor_id,
                    correlation_id: row.correlation_id,
                    message: r.message,
                });
            }
            Err(e) => {
                tracing::warn!("failed to render event {}: {e}", row.event_id);
                // Skip unrenderable events (e.g. unknown type from older versions)
            }
        }
    }

    Ok(Json(AuditFeedResponse { events }))
}
