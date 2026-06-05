use http::StatusCode;
use uuid::Uuid;
use schemars::JsonSchema;
use serde::Serialize;

use crate::audit::registry::{ViewerCtx, render};
use crate::auth::Subject;
use crate::auth::engine;
use crate::serve::AppState;

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

pub async fn fetch_events_for_entity(
    state: &AppState,
    subject: &Subject,
    entity_kind: &str,
    entity_id: Uuid,
) -> Result<AuditFeedResponse, StatusCode> {
    let valid_kinds = ["job", "host", "subject"];
    if !valid_kinds.contains(&entity_kind) {
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
        match entity_kind {
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
            "subject" => {
                // A user is entitled to the audit events about their own
                // account that are marked self-viewable (logins, profile
                // changes). Viewing another subject's feed without admin
                // authority yields no policies and is rejected below.
                if entity_id == viewer_id {
                    allowed_policies.push("self".to_string());
                }
            }
            _ => unreachable!(),
        }

        // If not admin and no allowed policies, they can't see anything.
        if allowed_policies.is_empty() {
            return Err(StatusCode::FORBIDDEN);
        }
    }

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
        entity_kind as _,
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
            }
        }
    }

    Ok(AuditFeedResponse { events })
}
