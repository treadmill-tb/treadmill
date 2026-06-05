//! Typed event vocabulary.
//!
//! Each event type in the audit log implements [`AuditEvent`]. The trait splits
//! the event's *intrinsic* identity (its versioned type string, the entities it
//! relates to, and the per-relation visibility) from its *instance* data (which
//! lives on the struct's fields and the [`AuditEvent::actor`] return value).
//! That split is what lets the read API enforce visibility purely from the
//! event type plus its stored [`Relation`] rows, with no per-event-type code
//! at query time.
//!
//! Phase 4 of the plan will introduce a `define_event!` macro that generates
//! the struct, the [`AuditEvent`] impl, the view-time renderer, and the
//! registry entry from a single declaration; the trait below is the contract
//! that macro will satisfy.

use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::auth::engine::{HostPermission, JobPermission};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Job(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Host(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Subject(pub Uuid);

/// Static identity + visibility of an audit event type, plus the instance
/// data needed to persist a single occurrence of it.
///
/// Implementations are written so that all three methods are derivable from
/// `self` alone -- the [`relations`](AuditEvent::relations) impl reads
/// entity ids out of the event struct's fields, so the event and its relations
/// cannot diverge. The `Serialize` super-trait is what [`persist_event`] uses
/// to lower the struct to the `jsonb` payload stored on `audit_events`.
///
/// [`persist_event`]: super::persist::persist_event
pub trait AuditEvent: Serialize {
    /// Stable, versioned discriminant -- e.g. `"job_finalized.v1"`. Stored
    /// verbatim in `audit_events.event_type` and used as the registry key when
    /// rendering the event at view time. Old versions are retained in the
    /// binary forever so historical rows remain renderable; bumping the suffix
    /// is how a payload-shape change ships without breaking the read API.
    fn event_type(&self) -> &'static str;

    /// The subject that caused this event. Worker-driven transitions with no
    /// human actor return [`SYSTEM_ACTOR_ID`](super::SYSTEM_ACTOR_ID).
    fn actor(&self) -> Uuid;

    /// Every entity this event touches, with the role it plays and the exact
    /// permission a viewer must hold on the entity to see the event through
    /// that relation. Visibility is disjunctive across relations -- a viewer
    /// sees the event if they satisfy *any* one of these policies.
    fn relations(&self) -> Vec<Relation>;
}

/// One way an event relates to a single entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relation {
    pub entity: EntityRef,
    pub role: Role,
    pub view: ViewPolicy,
}

/// A pointer to an entity an event relates to. The variants match
/// `tml_switchboard.audit_entity_kind` -- `Subject` covers both users and
/// groups, matching the unified subjects model in the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityRef {
    Job(Uuid),
    Host(Uuid),
    Subject(Uuid),
}

impl EntityRef {
    /// The textual `audit_entity_kind` enum value, used when writing the
    /// relation row.
    pub fn kind_str(self) -> &'static str {
        match self {
            EntityRef::Job(_) => "job",
            EntityRef::Host(_) => "host",
            EntityRef::Subject(_) => "subject",
        }
    }

    /// The underlying entity id, used as `audit_event_relations.entity_id`.
    pub fn id(self) -> Uuid {
        match self {
            EntityRef::Job(id) | EntityRef::Host(id) | EntityRef::Subject(id) => id,
        }
    }
}

/// The relation's role with respect to the event. Mirrors
/// `tml_switchboard.audit_role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The principal that caused the event. The actor's subject id is also
    /// carried directly on `audit_events.actor_id`; the relation row exists so
    /// the entity-scoped feed for a subject can surface the events they caused.
    Actor,
    /// The entity acted upon -- "the thing the event is about".
    Subject,
    /// Any additional entity whose viewers should also be able to see the
    /// event (e.g. the host on which a job ran).
    Context,
}

impl Role {
    /// The textual `audit_role` enum value, used when writing the relation row.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Actor => "actor",
            Role::Subject => "subject",
            Role::Context => "context",
        }
    }
}

/// Who is allowed to see an event through a particular relation. Stored as
/// text on `audit_event_relations.view_policy` so the same column covers both
/// host and job permission enums plus the operator-only sentinel.
///
/// Visibility matches the permission *exactly*: a viewer who holds
/// [`HostPermission::Manage`] does not implicitly see events whose policy is
/// [`HostPermission::Read`]. The read API computes the set of permissions the
/// viewer actually holds on the related entity and matches that set against
/// the stored policy (see plan §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewPolicy {
    /// Holders of this exact permission on the relation's entity may see the
    /// event.
    Permission(Permission),
    /// The subject the relation points at -- and only that subject -- may see
    /// the event (plus admins, who see everything). Used for a user's own
    /// account events such as logins and profile changes, where there is no
    /// permission grant to match against but the user is entitled to their own
    /// history. Only meaningful on a `Subject` relation.
    SelfAccess,
    /// System-internal: only members of the global-authority `admins` group
    /// ever see it. Never surfaces in a user-facing feed.
    OperatorOnly,
}

impl ViewPolicy {
    /// The textual value stored on `audit_event_relations.view_policy`.
    pub fn as_str(self) -> &'static str {
        match self {
            ViewPolicy::Permission(p) => p.as_str(),
            ViewPolicy::SelfAccess => "self",
            ViewPolicy::OperatorOnly => "operator_only",
        }
    }
}

/// A permission on some entity, irrespective of the entity's kind. The audit
/// layer needs a single type that spans both host and job permissions so
/// `ViewPolicy::Permission` can be uniformly stored in one text column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Host(HostPermission),
    Job(JobPermission),
}

impl Permission {
    /// The textual enum value (matches the underlying `host_permission` /
    /// `job_permission` enum values: `'read' | 'start' | 'ssh' | 'stop' |
    /// 'manage'`).
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Host(p) => p.as_str(),
            Permission::Job(p) => p.as_str(),
        }
    }
}

impl From<HostPermission> for Permission {
    fn from(p: HostPermission) -> Self {
        Permission::Host(p)
    }
}

impl From<JobPermission> for Permission {
    fn from(p: JobPermission) -> Self {
        Permission::Job(p)
    }
}
