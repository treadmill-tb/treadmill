//! Shared request/response types for the user-management REST API.
//!
//! These mirror the switchboard's `users` routes. The split between
//! [`PublicUserProfile`] and [`SelfUserProfile`] encodes the visibility rule:
//! anyone authenticated may read the public subset of any user, but only the
//! account owner sees the private additions (emails, group memberships, lock
//! state).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A linked GitHub identity, reduced to the fields safe to expose publicly.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct LinkedGitHub {
    /// The user's current GitHub login/handle.
    pub login: String,
    /// Canonical URL of the GitHub profile (`https://github.com/<login>`).
    pub profile_url: String,
}

/// The world-readable subset of a user profile: only data deemed safe to expose
/// to any authenticated caller.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct PublicUserProfile {
    pub user_id: Uuid,
    /// The user's display name: freely chosen, not unique.
    pub name: String,
    pub avatar_url: Option<String>,
    /// The linked GitHub account, if any.
    pub github: Option<LinkedGitHub>,
}

/// One email address on file for a user.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserEmail {
    pub email: String,
    /// Whether the provider has verified the address belongs to the user.
    pub verified: bool,
    /// Whether this is the user's primary address (set once at registration).
    pub is_primary: bool,
}

/// One group the user belongs to, including transitive memberships (e.g. via
/// nested groups or a linked GitHub org).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct GroupMembership {
    pub group_id: Uuid,
    pub name: String,
    /// How the membership arose (`direct`, `github_org`, ...).
    pub source: String,
    /// The external reference backing an auto-sourced membership (e.g. the
    /// GitHub org id); empty for direct memberships.
    pub source_ref: String,
}

/// The owner's full view of their own profile: the public subset plus the
/// private additions only the account holder may see.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct SelfUserProfile {
    #[serde(flatten)]
    pub profile: PublicUserProfile,
    /// All email addresses on file for the account, one entry per address.
    pub emails: Vec<UserEmail>,
    /// Every group the user belongs to, including transitive memberships.
    pub groups: Vec<GroupMembership>,
    /// Whether the account is locked (cannot log in or use existing sessions).
    pub locked: bool,
}

/// A patch to the caller's own profile. Omitting a field leaves it unchanged.
/// `name` cannot be cleared, only changed; an explicit `null` clears
/// `avatar_url`.
#[derive(schemars::JsonSchema, Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateProfileRequest {
    /// The user's display name: non-empty, bounded in length, no control
    /// characters, not unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_with::rust::double_option"
    )]
    #[schemars(with = "Option<String>")]
    pub avatar_url: Option<Option<String>>,
}

/// When and why a token was revoked.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct TokenRevocation {
    pub revoked_at: DateTime<Utc>,
    pub reason: String,
}

/// One of the caller's active or historical sessions/API tokens. Never carries
/// the secret token itself — only its metadata and provenance.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub token_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// The user agent that requested the token, if recorded.
    pub user_agent: Option<String>,
    /// An optional human-supplied label for the token.
    pub comment: Option<String>,
    /// The client IP the token was minted from, if recorded.
    pub created_ip: Option<String>,
    /// Set if the token has been revoked.
    pub revoked: Option<TokenRevocation>,
    /// True for the token used to authenticate the current request.
    pub current: bool,
}
