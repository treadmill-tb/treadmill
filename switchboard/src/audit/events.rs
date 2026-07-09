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

use crate::audit::model::{Host, ImageSet, Job, Subject};
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
    event_type = "user_logged_in";
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
    event_type = "user_provisioned";
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
    event_type = "oauth_identity_linked";
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
    event_type = "user_profile_changed";
    render = "profile updated on login";
}

define_event! {
    /// A verified email address was newly recorded for the user.
    UserEmailAdded v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        provider: String,
        email: String,
        verified: bool,
    }
    event_type = "user_email_added";
    render = "email {email} (verified = {verified:?}) added via {provider}";
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
    event_type = "group_membership_changed";
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
    event_type = "login_denied_locked";
    render = "login denied for locked account {login} via {provider}";
}

define_event! {
    /// An interactive login was refused by the admission gate before any user
    /// record was created. Operator-only: the denied party has no local account,
    /// so this is an operational signal rather than user-facing history. The
    /// actor is the well-known anonymous subject and there is no `user` relation
    /// (no user exists); the provider details and reason ride in the payload.
    RegistrationDenied v1 {
        actor: Subject,
        provider: String,
        provider_user_id: String,
        login: String,
        reason: String,
        client_ip: Option<String>,
        client_port: Option<i32>,
    }
    event_type = "registration_denied";
    render = "registration denied for {login} via {provider}: {reason}";
}

define_event! {
    /// A user accepted the Terms of Service at a given version, completing the
    /// ToS interstitial before a token was issued. Emitted on the re-acceptance
    /// path for an existing user whose accepted version had fallen behind (a
    /// brand-new user's first acceptance is implicit in `user_provisioned`). The
    /// affected user sees it via their own self-viewable relation.
    TosAccepted v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        version: i32,
    }
    event_type = "tos_accepted";
    render = "accepted terms of service version {version}";
}

define_event! {
    /// A user changed their own username via the management API. Carries the
    /// immutable `user_id` plus the old and new handle.
    UserRenamed v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        old_username: String,
        new_username: String,
    }
    event_type = "user_renamed";
    render = "renamed {old_username} to {new_username}";
}

define_event! {
    /// A user edited their own display name and/or avatar via the management
    /// API. Distinct from [`UserProfileChanged`], which records the implicit
    /// refresh of provider-sourced fields on login.
    UserProfileUpdated v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        old_full_name: Option<String>,
        new_full_name: Option<String>,
        old_avatar_url: Option<String>,
        new_avatar_url: Option<String>,
    }
    event_type = "user_profile_updated";
    render = "profile updated";
}

define_event! {
    /// A user revoked one of their own session/API tokens.
    SessionTokenRevoked v1 {
        actor: Subject,
        user: Subject @ view(SelfAccess),
        token_id: Uuid,
    }
    event_type = "session_token_revoked";
    render = "session token {token_id} revoked";
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
    event_type = "session_token_issued";
    render = "session token {token_id} issued";
}

define_event! {
    /// A user enqueued a new job (`POST /jobs`). Related to the job with the
    /// `read` policy, so it surfaces in the job's event feed for anyone who can
    /// read the job (its owner, a read-grantee, or an admin).
    JobEnqueued v1 {
        actor: Subject,
        job: Job @ view(Read),
    }
    event_type = "job_enqueued";
    render = "enqueued the job";
}

define_event! {
    /// A user requested termination of a job (`DELETE /jobs/{id}`). Visible to
    /// anyone who can read the job. `finalized_immediately` distinguishes a job
    /// canceled while still queued (finalized on the spot, no host involved)
    /// from a dispatched job whose stop the owning host's worker converges.
    JobTerminated v1 {
        actor: Subject,
        job: Job @ view(Read),
        finalized_immediately: bool,
    }
    event_type = "job_terminated";
    render = "requested job termination";
}

define_event! {
    /// The scheduler dispatched a queued job onto a host (`queued` → `assigned`).
    /// Attributed to the system actor; visible to anyone who can read the job and,
    /// as context, to viewers of the host it landed on.
    JobAssigned v1 {
        actor: Subject,
        job: Job @ view(Read),
        host: Host @ view(Read),
    }
    event_type = "job_assigned";
    render = "assigned the job to a host";
}

define_event! {
    /// A job reached a terminal state (`finalized`). `reason` is the recorded
    /// `termination_reason` (e.g. `workload_exited`, `execution_timeout`,
    /// `user_terminated`, `host_dropped_job`, `image_error`, ...). Attributed to
    /// the system actor; visible to job readers and, as context, to host viewers.
    JobFinalized v1 {
        actor: Subject,
        job: Job @ view(Read),
        host: Host @ view(Read),
        reason: String,
    }
    event_type = "job_finalized";
    render = "job finalized ({reason})";
}

define_event! {
    /// A supervisor opened (and authenticated) a WebSocket for its host, which
    /// the switchboard then marks live. Visible to host viewers.
    SupervisorConnected v1 {
        actor: Subject,
        host: Host @ view(Read),
    }
    event_type = "supervisor_connected";
    render = "supervisor connected";
}

define_event! {
    /// A supervisor's WebSocket closed and the host was marked not-live (the
    /// clean-disconnect path). Visible to host viewers.
    SupervisorDisconnected v1 {
        actor: Subject,
        host: Host @ view(Read),
    }
    event_type = "supervisor_disconnected";
    render = "supervisor disconnected";
}

define_event! {
    /// A concrete image was registered in the catalog, implicitly, by the first
    /// source added for its digest (`POST /images/{digest}/sources`). Related to
    /// the registering owner with the `self` policy so it surfaces in that
    /// user's own feed; the catalog has no per-image audit feed.
    ImageRegistered v1 {
        actor: Subject,
        owner: Subject @ view(SelfAccess),
        image_id: Uuid,
        manifest_digest: String,
    }
    event_type = "image_registered";
    render = "registered image {manifest_digest}";
}

define_event! {
    /// A new, empty image set was created (`POST /image-sets`). Visible to
    /// the set's managers and to the creating owner's own feed.
    ImageSetCreated v1 {
        actor: Subject,
        owner: Subject @ view(SelfAccess),
        set: ImageSet @ view(Manage),
        name: String,
    }
    event_type = "image_set_created";
    render = "created image set {name}";
}

define_event! {
    /// A full-replacement generation was appended to an image set
    /// (`POST /image-sets/{id}/generations`). Visible to the set's managers.
    ImageSetGenerationCreated v1 {
        actor: Subject,
        set: ImageSet @ view(Manage),
        generation: i64,
        member_count: i64,
    }
    event_type = "image_set_generation_created";
    render = "appended generation {generation} with {member_count} members";
}

define_event! {
    /// A `use`/`manage` grant on an image set was created
    /// (`POST /image-sets/{id}/grants`). Visible to the set's managers and,
    /// via the `self` policy, to the subject who received the grant.
    ImageSetGrantCreated v1 {
        actor: Subject,
        set: ImageSet @ view(Manage),
        grantee: Subject @ view(SelfAccess),
        permission: String,
    }
    event_type = "image_set_grant_created";
    render = "granted {permission} on the image set";
}

define_event! {
    /// A grant on an image set was revoked
    /// (`DELETE /image-sets/{id}/grants/...`). Visible to the set's managers
    /// and, via the `self` policy, to the subject whose grant was removed.
    ImageSetGrantRevoked v1 {
        actor: Subject,
        set: ImageSet @ view(Manage),
        grantee: Subject @ view(SelfAccess),
        permission: String,
    }
    event_type = "image_set_grant_revoked";
    render = "revoked {permission} on the image set";
}
