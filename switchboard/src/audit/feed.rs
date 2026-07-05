use base64::Engine as _;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::registry::{ViewerCtx, render};
use crate::auth::Subject;
use crate::auth::engine;
use crate::http_error::OrInternal;
use crate::serve::AppState;

// The wire types live in the shared `treadmill-rs` crate so HTTP clients (the
// web console, an eventual CLI) deserialize the exact structs we serialize here.
// Re-exported so the route handlers can keep importing them from this module.
pub use treadmill_rs::api::switchboard::audit::{AuditFeedResponse, RenderedAuditRow};

/// Default and maximum page sizes for an audit feed.
const DEFAULT_AUDIT_LIMIT: u32 = 50;
const MAX_AUDIT_LIMIT: u32 = 200;

/// Query parameters for an audit feed (`GET /{entity}/{id}/events`).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct AuditFeedQuery {
    /// Maximum number of events per page. Omitted or out-of-range values fall
    /// back to the server's default and bounds.
    limit: Option<u32>,
    /// Opaque keyset cursor from a previous response's `next_cursor`.
    cursor: Option<String>,
}

/// The keyset position encoded in an opaque feed `cursor`: the `(created_at,
/// event_id)` of the last row of the previous page.
#[derive(Serialize, Deserialize)]
struct AuditCursor {
    c: DateTime<Utc>,
    id: Uuid,
}

fn encode_cursor(created_at: DateTime<Utc>, event_id: Uuid) -> String {
    let json = serde_json::to_vec(&AuditCursor {
        c: created_at,
        id: event_id,
    })
    .expect("AuditCursor serializes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Decode an opaque feed cursor; `None` on any malformation (yielding a 400 at
/// the call site).
fn decode_cursor(cursor: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let parsed: AuditCursor = serde_json::from_slice(&bytes).ok()?;
    Some((parsed.c, parsed.id))
}

pub(crate) async fn fetch_events_for_entity(
    state: &AppState,
    subject: &Subject,
    entity_kind: &str,
    entity_id: Uuid,
    query: &AuditFeedQuery,
) -> Result<AuditFeedResponse, StatusCode> {
    let valid_kinds = ["job", "host", "subject", "image_group"];
    if !valid_kinds.contains(&entity_kind) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let viewer_id = subject.user_id();
    let is_admin = engine::is_admin(state.pool(), viewer_id)
        .await
        .or_internal("checking admin status")?;

    let mut allowed_policies: Vec<String> = Vec::new();

    if !is_admin {
        match entity_kind {
            "host" => {
                let perms = engine::host_permissions(state.pool(), viewer_id, entity_id)
                    .await
                    .or_internal("computing host permissions")?;
                allowed_policies.extend(perms.into_iter().map(|p| p.as_str().to_string()));
            }
            "job" => {
                let perms = engine::job_permissions(state.pool(), viewer_id, entity_id)
                    .await
                    .or_internal("computing job permissions")?;
                allowed_policies.extend(perms.into_iter().map(|p| p.as_str().to_string()));
            }
            "image_group" => {
                let perms = engine::image_group_permissions(state.pool(), viewer_id, entity_id)
                    .await
                    .or_internal("computing image-group permissions")?;
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

    let limit = query
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);

    let after = match query.cursor.as_deref() {
        Some(c) => Some(decode_cursor(c).ok_or(StatusCode::BAD_REQUEST)?),
        None => None,
    };
    let (after_created_at, after_event_id) = match after {
        Some((c, id)) => (Some(c), Some(id)),
        None => (None, None),
    };

    // Keyset on `(created_at, event_id)` descending; fetch one extra row to
    // learn whether a further (older) page exists. The relation filter is a
    // semi-join: an event may carry several matching relation rows for the
    // same entity (e.g. actor and subject both the viewer), and must still
    // appear once.
    let fetch = i64::from(limit) + 1;
    let mut rows = sqlx::query!(
        r#"
        select e.event_id, e.event_type, e.payload, e.actor_id, e.correlation_id, e.created_at
        from tml_switchboard.audit_events e
        where exists (
            select 1
            from tml_switchboard.audit_event_relations r
            where r.event_id = e.event_id
              and r.entity_kind = $1::tml_switchboard.audit_entity_kind
              and r.entity_id = $2
              and ($3::boolean or r.view_policy = any($4::text[]))
          )
          and ($5::timestamptz is null or (e.created_at, e.event_id) < ($5, $6))
        order by e.created_at desc, e.event_id desc
        limit $7
        "#,
        entity_kind as _,
        entity_id,
        is_admin,
        &allowed_policies,
        after_created_at,
        after_event_id,
        fetch,
    )
    .fetch_all(state.pool())
    .await
    .or_internal("fetching audit events")?;

    // If the extra row came back, there is another page; the cursor points at
    // the last row we keep. Rendering below may drop individual rows, but
    // pagination is by DB row position, so the cursor stays correct.
    let next_cursor = if rows.len() as i64 > i64::from(limit) {
        rows.truncate(limit as usize);
        rows.last().map(|r| encode_cursor(r.created_at, r.event_id))
    } else {
        None
    };

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

    Ok(AuditFeedResponse {
        events,
        next_cursor,
    })
}
