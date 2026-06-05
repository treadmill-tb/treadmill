//! Concrete audit event vocabulary.
//!
//! Every event is declared with [`define_event!`](crate::define_event), which
//! generates the payload struct, its [`AuditEvent`](crate::audit::model::AuditEvent)
//! impl, and the view-time renderer registry entry. The login-flow events below
//! all carry the immutable internal `user_id` (via the `user` relation field) so
//! a row remains attributable even after the provider handle or email changes,
//! and all mark that relation `SelfAccess` so the user can see their own history.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::audit::model::Subject;
use crate::define_event;

define_event! {
    /// A user completed an interactive OAuth login and was issued a session
    /// token. Emitted once per successful callback, after provisioning.
    UserLoggedIn v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        provider: String,
        provider_user_id: String,
        login: String,
        new_user: bool,
        client_ip: Option<String>,
        client_port: Option<i32>,
    }
    render = "{login} logged in via {provider}";
}

define_event! {
    /// A brand-new local account was created from an external identity.
    UserProvisioned v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        provider: String,
        provider_user_id: String,
        login: String,
        username: String,
    }
    render = "provisioned user {username} from {provider} identity {login}";
}

define_event! {
    /// An existing user was matched to a new external identity by a shared
    /// verified email and that identity was linked to their account.
    OAuthIdentityLinked v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        provider: String,
        provider_user_id: String,
        login: String,
    }
    render = "linked {provider} identity {login} to existing account";
}

define_event! {
    /// One or more mutable profile fields changed during a login refresh.
    /// Emitted only when a value actually differs (compare-then-write), so a
    /// no-op re-login does not spam the log. Carries the prior and new values.
    UserProfileChanged v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        old_full_name: Option<String>,
        new_full_name: Option<String>,
        old_avatar_url: Option<String>,
        new_avatar_url: Option<String>,
        old_provider_login: Option<String>,
        new_provider_login: Option<String>,
    }
    render = "profile updated on login";
}

define_event! {
    /// A verified email address was newly recorded for the user.
    UserEmailAdded v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        provider: String,
        email: String,
    }
    render = "verified email {email} added via {provider}";
}

define_event! {
    /// A `github_org`-sourced group membership was added or removed during
    /// auto-group reconciliation. The group relation is operator-visible; the
    /// affected user sees the event through their own self-viewable relation.
    GroupMembershipChanged v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        group: Subject @ view(OperatorOnly),
        source_ref: String,
        added: bool,
    }
    render = "github_org membership {source_ref} updated";
}

define_event! {
    /// A login was refused because the resolved account is locked. Operator-only:
    /// the locked user cannot authenticate to view their own feed, and a refused
    /// attempt is an operational signal rather than user-facing history.
    LoginDeniedLocked v1 {
        actor: Subject,
        user: Subject @ view(Operator),
        provider: String,
        provider_user_id: String,
        login: String,
        client_ip: Option<String>,
        client_port: Option<i32>,
    }
    render = "login denied for locked account {login} via {provider}";
}

define_event! {
    /// A session/API token was minted for the user.
    SessionTokenIssued v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        token_id: Uuid,
        expires_at: DateTime<Utc>,
        client_ip: Option<String>,
        client_port: Option<i32>,
        user_agent: Option<String>,
    }
    render = "session token {token_id} issued";
}
