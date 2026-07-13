//! User-management REST routes: self-profile read/update, public profile view,
//! session/token listing and revocation, and the per-user audit feed.

use axum::Json;
use axum::extract::Path;
use axum::extract::{Query, State};
use http::StatusCode;
use uuid::Uuid;

use treadmill_rs::api::switchboard::users::{
    GroupMembership, LinkedGitHub, PublicUserProfile, SelfUserProfile, SessionInfo,
    TokenRevocation, UpdateProfileRequest,
};

use crate::audit;
use crate::audit::feed::{AuditFeedQuery, AuditFeedResponse, fetch_events_for_entity};
use crate::http_error::internal;
use crate::routes::params::{IdPath, TokenIdPath};
use crate::serve::AppState;
use crate::sql::api_token::{self, RevokeToken};
use crate::sql::user::UpdateUserProfile;

/// A display name is the trimmed input: non-empty, at most 256 characters, and
/// free of control characters. Any other Unicode is allowed; nothing routes on
/// it, so there is no uniqueness or reserved-name check.
fn name_valid(s: &str) -> bool {
    let n = s.chars().count();
    (1..=256).contains(&n) && !s.chars().any(char::is_control)
}

/// An avatar URL must parse and be an http(s) URL of bounded length.
fn avatar_url_valid(s: &str) -> bool {
    if s.len() > 2048 {
        return false;
    }
    match url::Url::parse(s) {
        Ok(u) => matches!(u.scheme(), "http" | "https"),
        Err(_) => false,
    }
}

/// Build the public GitHub view from the recorded login handle.
fn linked_github(login: Option<String>) -> Option<LinkedGitHub> {
    login.map(|login| {
        let profile_url = format!("https://github.com/{login}");
        LinkedGitHub { login, profile_url }
    })
}

/// The current GitHub login handle for a user, if they have a linked identity.
async fn github_login(state: &AppState, user_id: Uuid) -> Result<Option<String>, sqlx::Error> {
    let login = sqlx::query_scalar!(
        "select provider_login from tml_switchboard.user_identities \
         where user_id = $1 and provider = 'github';",
        user_id,
    )
    .fetch_optional(state.pool())
    .await?;
    // `fetch_optional` -> Option<row>; `provider_login` itself is nullable.
    Ok(login.flatten())
}

/// Assemble the owner's full view of their own account.
async fn load_self_profile(state: &AppState, user_id: Uuid) -> Result<SelfUserProfile, StatusCode> {
    let row = sqlx::query!(
        "select name, avatar_url, locked \
         from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_optional(state.pool())
    .await
    .map_err(internal)?
    .ok_or(StatusCode::NOT_FOUND)?;

    let github = linked_github(github_login(state, user_id).await.map_err(internal)?);

    let emails = sqlx::query_scalar!(
        "select email from tml_switchboard.user_emails where user_id = $1 order by added_at;",
        user_id,
    )
    .fetch_all(state.pool())
    .await
    .map_err(internal)?;

    // Every group the user reaches transitively. `principals()` returns the
    // user's own subject id plus every group it belongs to; joining to `groups`
    // drops the self row. The left join recovers provenance for direct
    // memberships; a purely transitive group has no membership row, reported as
    // `transitive`.
    let group_rows = sqlx::query!(
        r#"select g.subject_id as "group_id!", g.name as "name!",
                  coalesce(gm.source::text, 'transitive') as "source!",
                  coalesce(gm.source_ref, '') as "source_ref!"
           from tml_switchboard.principals($1::uuid) p
           join tml_switchboard.groups g on g.subject_id = p.id
           left join tml_switchboard.group_members gm
             on gm.group_id = g.subject_id and gm.member_id = $1::uuid
           order by g.name;"#,
        user_id,
    )
    .fetch_all(state.pool())
    .await
    .map_err(internal)?;

    let groups = group_rows
        .into_iter()
        .map(|r| GroupMembership {
            group_id: r.group_id,
            name: r.name,
            source: r.source,
            source_ref: r.source_ref,
        })
        .collect();

    Ok(SelfUserProfile {
        profile: PublicUserProfile {
            user_id,
            name: row.name,
            avatar_url: row.avatar_url,
            github,
        },
        emails,
        groups,
        locked: row.locked,
    })
}

/// `GET /users/me`: the caller's own profile, including private additions.
pub async fn get_me(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<SelfUserProfile>, StatusCode> {
    load_self_profile(&state, subject.user_id()).await.map(Json)
}

/// `PATCH /users/me`: update the caller's display name and/or avatar.
pub async fn patch_me(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<Json<SelfUserProfile>, StatusCode> {
    let user_id = subject.user_id();

    // Validate inputs up front (cheap, no DB round-trips). The display name is
    // stored trimmed.
    let new_name = req.name.map(|n| n.trim().to_string());
    if let Some(name) = &new_name
        && !name_valid(name)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(Some(avatar)) = &req.avatar_url
        && !avatar_url_valid(avatar)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut tx = state.pool().begin().await.map_err(internal)?;

    let current = sqlx::query!(
        "select name, avatar_url \
         from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;

    // Display name / avatar update, only when something was supplied and the
    // final value differs from what is on file (avoids no-op audit rows).
    if new_name.is_some() || req.avatar_url.is_some() {
        let new_name = new_name.unwrap_or_else(|| current.name.clone());
        let new_avatar_url = req.avatar_url.unwrap_or_else(|| current.avatar_url.clone());
        if new_name != current.name || new_avatar_url != current.avatar_url {
            audit::transition(
                &mut tx,
                UpdateUserProfile {
                    user_id,
                    old_name: current.name.clone(),
                    new_name,
                    old_avatar_url: current.avatar_url.clone(),
                    new_avatar_url,
                },
            )
            .await
            .map_err(internal)?;
        }
    }

    tx.commit().await.map_err(internal)?;

    load_self_profile(&state, user_id).await.map(Json)
}

/// `GET /users/{id}`: any authenticated caller may read the safe public subset
/// of another user's profile.
pub async fn get_user(
    State(state): State<AppState>,
    _subject: crate::auth::Subject,
    Path(IdPath { id: user_id }): Path<IdPath>,
) -> Result<Json<PublicUserProfile>, StatusCode> {
    let row = sqlx::query!(
        "select name, avatar_url \
         from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_optional(state.pool())
    .await
    .map_err(internal)?
    .ok_or(StatusCode::NOT_FOUND)?;

    let github = linked_github(github_login(&state, user_id).await.map_err(internal)?);

    Ok(Json(PublicUserProfile {
        user_id,
        name: row.name,
        avatar_url: row.avatar_url,
        github,
    }))
}

/// `GET /users/me/tokens`: list the caller's sessions/API tokens, flagging the
/// one used to authenticate this request.
pub async fn list_tokens(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<Vec<SessionInfo>>, StatusCode> {
    let user_id = subject.user_id();
    let current_token = subject.token_id();

    let rows = sqlx::query!(
        r#"select token_id, created_at, expires_at, user_agent, comment, created_ip,
                  revoked as "revoked: crate::sql::api_token::Revocation"
           from tml_switchboard.api_tokens where user_id = $1
           order by created_at desc, token_id desc;"#,
        user_id,
    )
    .fetch_all(state.pool())
    .await
    .map_err(internal)?;

    let sessions = rows
        .into_iter()
        .map(|r| SessionInfo {
            token_id: r.token_id,
            created_at: r.created_at,
            expires_at: r.expires_at,
            user_agent: r.user_agent,
            comment: r.comment,
            created_ip: r.created_ip,
            revoked: r.revoked.map(|c| TokenRevocation {
                revoked_at: c.revoked_at,
                reason: c.revocation_reason,
            }),
            current: r.token_id == current_token,
        })
        .collect();

    Ok(Json(sessions))
}

/// `DELETE /users/me/tokens/{token_id}`: revoke one of the caller's own tokens.
pub async fn revoke_token(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(TokenIdPath { token_id }): Path<TokenIdPath>,
) -> Result<StatusCode, StatusCode> {
    let user_id = subject.user_id();

    // Confirm the token exists and is the caller's before revoking it. A token
    // owned by someone else is reported as not-found so its existence does not
    // leak to a non-owner.
    let meta = match api_token::fetch_metadata_by_id(state.pool(), token_id).await {
        Ok(m) => m,
        Err(api_token::TokenError::InvalidToken) => return Err(StatusCode::NOT_FOUND),
        Err(e) => return Err(internal(e)),
    };
    if meta.user_id != user_id {
        return Err(StatusCode::NOT_FOUND);
    }

    let mut tx = state.pool().begin().await.map_err(internal)?;
    audit::transition(
        &mut tx,
        RevokeToken {
            user_id,
            token_id,
            reason: "revoked by user".to_string(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /users/{id}/events`: the per-user audit feed (self/admin only).
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: user_id }): Path<IdPath>,
    Query(query): Query<AuditFeedQuery>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "subject", user_id, &query)
        .await
        .map(Json)
}
