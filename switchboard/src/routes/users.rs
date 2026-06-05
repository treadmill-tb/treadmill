//! User-management REST routes: self-profile read/update, public profile view,
//! session/token listing and revocation, and the per-user audit feed.

use axum::Json;
use axum::extract::{Path, State};
use http::StatusCode;
use uuid::Uuid;

use treadmill_rs::api::switchboard::users::{
    GroupMembership, LinkedGitHub, PublicUserProfile, SelfUserProfile, SessionInfo,
    TokenCancellation, UpdateProfileRequest,
};

use crate::audit;
use crate::audit::feed::{AuditFeedResponse, fetch_events_for_entity};
use crate::serve::AppState;
use crate::sql::api_token::{self, RevokeToken};
use crate::sql::user::{RenameUser, UpdateUserProfile};

/// Usernames that may never be claimed: route literals and well-known names that
/// would be confusing or dangerous as a handle.
const RESERVED_USERNAMES: &[&str] = &[
    "me", "admin", "admins", "administrator", "system", "root", "new", "tokens", "events", "users",
    "auth", "api",
];

/// Map any error to a 500, logging it. Used for unexpected DB failures.
fn internal(e: impl std::fmt::Display) -> StatusCode {
    tracing::error!("user route internal error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Enforce `^[a-z0-9][a-z0-9-]{1,38}$`: 2..=39 chars, leading alphanumeric, rest
/// lowercase-alphanumeric or hyphen.
fn username_valid_shape(s: &str) -> bool {
    let len = s.len();
    if !(2..=39).contains(&len) {
        return false;
    }
    let mut bytes = s.bytes();
    let first = bytes.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
        "select username, full_name, avatar_url, locked \
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
            username: row.username,
            full_name: row.full_name,
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

/// `PATCH /users/me`: update the caller's display name, username, and/or avatar.
pub async fn patch_me(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<Json<SelfUserProfile>, StatusCode> {
    let user_id = subject.user_id();

    // Validate inputs up front (cheap, no DB round-trips).
    if let Some(new_username) = &req.username
        && (!username_valid_shape(new_username)
            || RESERVED_USERNAMES.contains(&new_username.as_str()))
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
        "select username, full_name, avatar_url \
         from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;

    // Username rename, only when it actually changes. A collision with another
    // user's handle surfaces as a unique violation, reported as a clean 409.
    if let Some(new_username) = req.username
        && new_username != current.username
        && let Err(e) = audit::transition(
            &mut tx,
            RenameUser {
                user_id,
                old_username: current.username.clone(),
                new_username,
            },
        )
        .await
    {
        return Err(match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => StatusCode::CONFLICT,
            _ => internal(e),
        });
    }

    // Display name / avatar update, only when something was supplied and the
    // final value differs from what is on file (avoids no-op audit rows).
    if req.full_name.is_some() || req.avatar_url.is_some() {
        let new_full_name = req.full_name.unwrap_or_else(|| current.full_name.clone());
        let new_avatar_url = req.avatar_url.unwrap_or_else(|| current.avatar_url.clone());
        if new_full_name != current.full_name || new_avatar_url != current.avatar_url {
            audit::transition(
                &mut tx,
                UpdateUserProfile {
                    user_id,
                    old_full_name: current.full_name.clone(),
                    new_full_name,
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
    Path(user_id): Path<Uuid>,
) -> Result<Json<PublicUserProfile>, StatusCode> {
    let row = sqlx::query!(
        "select username, full_name, avatar_url \
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
        username: row.username,
        full_name: row.full_name,
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
                  canceled as "canceled: crate::sql::api_token::Cancellation"
           from tml_switchboard.api_tokens where user_id = $1 order by created_at desc;"#,
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
            canceled: r.canceled.map(|c| TokenCancellation {
                canceled_at: c.canceled_at,
                reason: c.cancellation_reason,
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
    Path(token_id): Path<Uuid>,
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
    Path(user_id): Path<Uuid>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "subject", user_id)
        .await
        .map(Json)
}
