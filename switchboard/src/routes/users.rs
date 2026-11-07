//! User-management REST routes: self-profile read/update, public profile view,
//! session/token listing and revocation, and the per-user audit feed.

use axum::Json;
use axum::extract::Path;
use axum::extract::{Query, State};
use http::StatusCode;
use uuid::Uuid;

use http::request::Parts;
use treadmill_rs::api::switchboard::users::{
    CreateUserRequest, GroupMembership, IssueTokenRequest, IssuedToken, LinkedGitHub,
    PublicUserProfile, SelfUserProfile, SessionInfo, TokenRevocation, UpdateProfileRequest,
    UserEmail,
};

use crate::audit;
use crate::audit::feed::{AuditFeedQuery, AuditFeedResponse, fetch_events_for_entity};
use crate::auth::engine;
use crate::http_error::internal;
use crate::routes::auth::ClientContext;
use crate::routes::params::{IdPath, UserPath, UserTokenPath};
use crate::serve::AppState;
use crate::sql::api_token::{self, IssueBotToken, RevokeToken};
use crate::sql::user::{CreateBot, UpdateUserProfile};

/// The longest accepted token comment, in characters.
const MAX_TOKEN_COMMENT_CHARS: usize = 256;

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
        "select name, avatar_url, locked, bot \
         from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_optional(state.pool())
    .await
    .map_err(internal)?
    .ok_or(StatusCode::NOT_FOUND)?;

    let github = linked_github(github_login(state, user_id).await.map_err(internal)?);

    // One entry per address: an address may be on file under several providers
    // (notably the provider-less primary alongside its provider-scoped copy), so
    // collapse the rows -- verified if any copy is, primary if the provider-less
    // copy is present.
    let emails = sqlx::query!(
        "select email, bool_or(verified) as \"verified!\", \
                bool_or(provider = '') as \"is_primary!\" \
         from tml_switchboard.user_emails where user_id = $1 \
         group by email order by min(added_at);",
        user_id,
    )
    .fetch_all(state.pool())
    .await
    .map_err(internal)?
    .into_iter()
    .map(|r| UserEmail {
        email: r.email,
        verified: r.verified,
        is_primary: r.is_primary,
    })
    .collect();

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
            bot: row.bot,
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
        "select name, avatar_url, bot \
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
        bot: row.bot,
    }))
}

/// Every token `user_id` holds, newest first. `current` flags the token the
/// request authenticated with, if it is among them.
async fn sessions_of(
    state: &AppState,
    user_id: Uuid,
    current: Option<Uuid>,
) -> Result<Vec<SessionInfo>, StatusCode> {
    let rows = sqlx::query!(
        r#"select token_id, created_at, expires_at, user_agent, comment, created_ip,
                  revoked as "revoked: crate::sql::api_token::Revocation"
           from tml_switchboard.api_tokens where subject_id = $1
           order by created_at desc, token_id desc;"#,
        user_id,
    )
    .fetch_all(state.pool())
    .await
    .map_err(internal)?;

    Ok(rows
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
            current: Some(r.token_id) == current,
        })
        .collect())
}

/// Revoke `user_id`'s token `token_id` on `actor`'s behalf, once the caller is
/// known to be allowed to. A token that does not exist or belongs to someone
/// else is reported as not-found, so its existence does not leak.
async fn revoke(
    state: &AppState,
    actor: Uuid,
    user_id: Uuid,
    token_id: Uuid,
) -> Result<StatusCode, StatusCode> {
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
            actor,
            user_id,
            token_id,
            reason: if actor == user_id {
                "revoked by user"
            } else {
                "revoked by admin"
            }
            .to_string(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

async fn is_admin(state: &AppState, subject: Uuid) -> Result<bool, StatusCode> {
    engine::is_admin(state.pool(), subject)
        .await
        .map_err(internal)
}

/// Whether the user exists and is a bot; `None` if there is no such user.
async fn user_is_bot(state: &AppState, user_id: Uuid) -> Result<Option<bool>, StatusCode> {
    sqlx::query_scalar!(
        "select bot from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_optional(state.pool())
    .await
    .map_err(internal)
}

/// Require that the caller may see and revoke `user_id`'s tokens: they are
/// that user, or a global admin (for any user, human or bot). Anything else --
/// including an unknown user -- is reported as not-found, so it does not leak
/// which users exist.
async fn require_self_or_admin(
    state: &AppState,
    subject: &crate::auth::Subject,
    user_id: Uuid,
) -> Result<(), StatusCode> {
    if subject.user_id() == user_id {
        return Ok(());
    }
    if is_admin(state, subject.user_id()).await? && user_is_bot(state, user_id).await?.is_some() {
        return Ok(());
    }
    Err(StatusCode::NOT_FOUND)
}

/// `POST /users`: create a bot user. Global admins only; human accounts are
/// provisioned by interactive login, so `bot` must be true.
pub async fn create_user(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<PublicUserProfile>), StatusCode> {
    if !is_admin(&state, subject.user_id()).await? {
        return Err(StatusCode::FORBIDDEN);
    }
    if !req.bot {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let name = req.name.trim().to_string();
    if !name_valid(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut tx = state.pool().begin().await.map_err(internal)?;
    let user_id = audit::transition(
        &mut tx,
        CreateBot {
            actor: subject.user_id(),
            name: name.clone(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(PublicUserProfile {
            user_id,
            name,
            avatar_url: None,
            github: None,
            bot: true,
        }),
    ))
}

/// `POST /users/{id}/tokens`: mint an API token for a bot user. Global admins
/// only, and never for a human: that would let an admin impersonate them. The
/// secret is in the response only; it cannot be retrieved again.
pub async fn issue_token(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(UserPath { id }): Path<UserPath>,
    parts: Parts,
    Json(req): Json<IssueTokenRequest>,
) -> Result<(StatusCode, Json<IssuedToken>), StatusCode> {
    if !is_admin(&state, subject.user_id()).await? {
        return Err(StatusCode::FORBIDDEN);
    }
    let user_id = id.resolve(subject.user_id());
    match user_is_bot(&state, user_id).await? {
        None => return Err(StatusCode::NOT_FOUND),
        Some(false) => return Err(StatusCode::UNPROCESSABLE_ENTITY),
        Some(true) => {}
    }

    if let Some(expires_at) = req.expires_at
        && expires_at <= chrono::Utc::now()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let comment = req
        .comment
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    if let Some(c) = &comment
        && (c.chars().count() > MAX_TOKEN_COMMENT_CHARS || c.chars().any(char::is_control))
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let ctx = ClientContext::resolve(&parts, &state.config().server);
    let mut tx = state.pool().begin().await.map_err(internal)?;
    let (token_id, token) = audit::transition(
        &mut tx,
        IssueBotToken {
            actor: subject.user_id(),
            bot_id: user_id,
            expires_at: req.expires_at,
            comment,
            created_ip: ctx.ip,
            created_port: ctx.port,
            user_agent: ctx.user_agent,
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(IssuedToken {
            token_id,
            token: token.into(),
            expires_at: req.expires_at,
        }),
    ))
}

/// `GET /users/{id}/tokens`: list a user's sessions/API tokens (metadata only),
/// flagging the one used to authenticate this request. The user themself
/// (`/users/me/tokens`) or a global admin.
pub async fn list_tokens(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(UserPath { id }): Path<UserPath>,
) -> Result<Json<Vec<SessionInfo>>, StatusCode> {
    let user_id = id.resolve(subject.user_id());
    require_self_or_admin(&state, &subject, user_id).await?;
    sessions_of(&state, user_id, Some(subject.token_id()))
        .await
        .map(Json)
}

/// `DELETE /users/{id}/tokens/{token_id}`: revoke a user's token. The user
/// themself (`/users/me/tokens/{token_id}`) or a global admin.
pub async fn revoke_token(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(UserTokenPath { id, token_id }): Path<UserTokenPath>,
) -> Result<StatusCode, StatusCode> {
    let user_id = id.resolve(subject.user_id());
    require_self_or_admin(&state, &subject, user_id).await?;
    revoke(&state, subject.user_id(), user_id, token_id).await
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
