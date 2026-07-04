//! Profile pages: the caller's own `/me` and the public `/users/{id}`.
//!
//! Both render live from the typed switchboard client and, per the console's
//! convention, end with the head of the resource's audit log.

use axum::extract::{Path, State};
use axum::response::Redirect;
use maud::{Markup, html};
use treadmill_rs::api::switchboard::audit::RenderedAuditRow;
use treadmill_rs::api::switchboard::client::ClientError;
use treadmill_rs::api::switchboard::users::{PublicUserProfile, SelfUserProfile, SessionInfo};
use uuid::Uuid;

use crate::serve::AppState;
use crate::session::Session;
use crate::views::audit::audit_feed;
use crate::views::{PageError, layout, timestamp};

/// `GET /` — send visitors to their profile (or, if logged out, the session
/// extractor on `/me` will bounce them to `/login`).
pub async fn index() -> Redirect {
    Redirect::to("/me")
}

/// `GET /me` — the caller's own profile, sessions, and audit feed.
pub async fn me(session: Session) -> Result<Markup, PageError> {
    let profile = session.client.get_me().await?;
    let tokens = session.client.list_my_tokens().await?;
    let events = session
        .client
        .user_events(profile.profile.user_id)
        .await?
        .events;

    let username = profile.profile.username.clone();
    Ok(layout(
        "Profile",
        Some(&username),
        html! {
            h1 { "Your profile" }
            (self_profile_card(&profile))
            (sessions_section(&tokens))
            (audit_feed(&events))
        },
    ))
}

/// `GET /users/{id}` — the public view of another user. The audit feed is shown
/// only when the viewer is permitted to read it (self/admin); otherwise it is
/// quietly omitted.
pub async fn user(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<Uuid>,
) -> Result<Markup, PageError> {
    let profile = session.client.get_user(id).await?;
    let events = optional_feed(&session, id).await?;
    let viewer = nav_user(&state, &session).await;

    Ok(layout(
        &profile.username,
        viewer.as_deref(),
        html! {
            h1 { (profile.username) }
            (public_profile_card(&profile))
            @if let Some(events) = &events {
                (audit_feed(events))
            }
        },
    ))
}

/// Fetch a user's audit feed, mapping a `403` (viewer not permitted) to `None`
/// rather than an error so the page still renders.
async fn optional_feed(
    session: &Session,
    id: Uuid,
) -> Result<Option<Vec<RenderedAuditRow>>, PageError> {
    match session.client.user_events(id).await {
        Ok(feed) => Ok(Some(feed.events)),
        Err(ClientError::Status { status: 403, .. }) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

/// The logged-in user's handle for the nav bar, or `None` if the lookup fails.
async fn nav_user(_state: &AppState, session: &Session) -> Option<String> {
    session.client.whoami().await.ok().map(|w| w.username)
}

fn self_profile_card(profile: &SelfUserProfile) -> Markup {
    let p = &profile.profile;
    html! {
        section.card {
            dl.fields {
                @if let Some(avatar) = &p.avatar_url {
                    dt { "Avatar" }
                    dd { img src=(avatar) alt="avatar" width="48" height="48"; }
                }
                dt { "Username" }
                dd { (p.username) }
                dt { "Full name" }
                dd { @match &p.full_name {
                    Some(name) => (name),
                    None => span.muted { "—" },
                } }
                dt { "User ID" }
                dd { code { (p.user_id) } }
                @if let Some(gh) = &p.github {
                    dt { "GitHub" }
                    dd { a href=(gh.profile_url) { (gh.login) } }
                }
                dt { "Emails" }
                dd { @if profile.emails.is_empty() {
                    span.muted { "—" }
                } @else {
                    (profile.emails.join(", "))
                } }
                dt { "Status" }
                dd { @if profile.locked {
                    span.tag { "locked" }
                } @else {
                    "active"
                } }
            }
            @if !profile.groups.is_empty() {
                h2 { "Groups" }
                table {
                    thead { tr { th { "Group" } th { "Source" } } }
                    tbody {
                        @for g in &profile.groups {
                            tr {
                                td { a href=(format!("/groups/{}", g.group_id)) { (g.name) } }
                                td { code { (g.source) } }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn public_profile_card(profile: &PublicUserProfile) -> Markup {
    html! {
        section.card {
            dl.fields {
                @if let Some(avatar) = &profile.avatar_url {
                    dt { "Avatar" }
                    dd { img src=(avatar) alt="avatar" width="48" height="48"; }
                }
                dt { "Username" }
                dd { (profile.username) }
                dt { "Full name" }
                dd { @match &profile.full_name {
                    Some(name) => (name),
                    None => span.muted { "—" },
                } }
                dt { "User ID" }
                dd { code { (profile.user_id) } }
                @if let Some(gh) = &profile.github {
                    dt { "GitHub" }
                    dd { a href=(gh.profile_url) { (gh.login) } }
                }
            }
        }
    }
}

fn sessions_section(tokens: &[SessionInfo]) -> Markup {
    html! {
        h2 { "Sessions" }
        section.card {
            @if tokens.is_empty() {
                p.empty { "No active sessions." }
            } @else {
                table {
                    thead {
                        tr {
                            th { "Created" }
                            th { "Expires" }
                            th { "Client" }
                            th { "" }
                        }
                    }
                    tbody {
                        @for t in tokens {
                            tr {
                                td { (timestamp(t.created_at)) }
                                td { (timestamp(t.expires_at)) }
                                td {
                                    @match &t.user_agent {
                                        Some(ua) => (ua),
                                        None => span.muted { "—" },
                                    }
                                    @if let Some(ip) = &t.created_ip {
                                        " " span.muted { "(" (ip) ")" }
                                    }
                                }
                                td {
                                    @if t.current { span.tag.current { "current" } }
                                    @if let Some(rev) = &t.revoked {
                                        span.tag { "revoked" }
                                        " " span.muted { (rev.reason) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
