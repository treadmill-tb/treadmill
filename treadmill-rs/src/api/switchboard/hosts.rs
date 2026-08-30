//! Host-scoped client API types.
//!
//! A host's description is its [`HostSpec`]; these types carry the operational
//! state around it — liveness, maintenance — and none of the supervisor
//! credentials or worker bookkeeping on the underlying row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::switchboard::JobInitSpec;
use crate::host_spec::HostSpec;

/// A host as returned by `GET /hosts` and `GET /hosts/{id}`: its operational
/// state plus the admin-authored spec describing what it is.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub host_id: Uuid,
    pub name: String,
    /// Whether the host's supervisor has heartbeat recently enough to be
    /// considered schedulable, computed with the deployment's liveness window.
    pub live: bool,
    /// The host's last heartbeat, or null if it has never reported (or its
    /// supervisor disconnected cleanly).
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Whether an operator has withheld this host from scheduling. A host in
    /// maintenance is neither dispatched onto nor preempted to free capacity.
    pub maintenance: bool,
    /// The host's current spec, normalized to the latest version. Null only for
    /// a host that has never been described.
    pub spec: Option<HostSpec>,
    /// The revision `spec` was read at, for `If-Match` on a spec write. Null
    /// exactly when `spec` is.
    pub spec_revision: Option<i32>,
}

/// A change to a host's operational state, carried by `PATCH /hosts/{id}`.
///
/// Only the fields present are changed. Host *description* is not editable
/// here: it lives in the host's spec, which is versioned separately.
#[derive(schemars::JsonSchema, Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostUpdateRequest {
    /// Withhold the host from scheduling, or return it to service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<bool>,
}

/// A new host (`POST /hosts`): the `hosts` row and revision 1 of its spec are
/// written in one transaction, so a host is never in an undescribed state.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCreateRequest {
    /// The host's spec. Its `id` becomes the host's id: the client supplies the
    /// UUID so a spec is a self-contained document that can live in a git repo
    /// and be applied. Rejected with a [`HostSpecRejection`] naming the
    /// offending field if it does not validate.
    #[schemars(with = "HostSpec")]
    pub spec: serde_json::Value,
}

/// The created host, and the credential its supervisor authenticates with.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostCreateResponse {
    pub host_id: Uuid,
    /// Base64 bearer token for the host's `/hosts/{id}/connect` WebSocket. The
    /// API never returns it again, so it has to be captured here.
    pub auth_token: String,
    /// The revision the spec was stored at, always the first.
    pub spec_revision: i32,
}

/// A new revision of a host's spec (`PUT /hosts/{id}/spec`).
///
/// The write is conditional on an `If-Match` header carrying the revision the
/// caller last read, so two admins editing one host cannot silently clobber
/// each other.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSpecUpdateRequest {
    /// The replacement spec. Its `id` must be the host being written.
    #[schemars(with = "HostSpec")]
    pub spec: serde_json::Value,
}

/// The outcome of a spec write.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostSpecUpdateResponse {
    /// The revision the document was stored at; the next write's `If-Match`.
    pub spec_revision: i32,
}

/// Why a submitted host spec was refused (`422 Unprocessable Entity`).
///
/// Specs are hand-edited documents, so a rejection names the offending field
/// rather than a byte offset.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostSpecRejection {
    /// Dotted path to the offending field, e.g. `duts[0].debug.probe.serial`.
    /// Empty when the fault is the document as a whole.
    pub path: String,
    /// Human-readable explanation; not intended to be parsed.
    pub message: String,
}

/// A dry run of a job's host requirements (`POST /host-requirements/validate`).
///
/// Answers the question a queued job cannot: *would this ever be placed?*
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRequirementsRequest {
    /// The predicate to evaluate, as `JobRequest::host_cel_predicate`.
    pub host_cel_predicate: String,
    /// The image the job would run, evaluated exactly as an enqueue would
    /// resolve it. Supplying it separates the two ways a job goes unplaced —
    /// the predicate matched nothing, or no image-set member admits the hosts
    /// it did match — which look identical from a job sitting queued. Omitted,
    /// only the predicate is evaluated.
    #[serde(default)]
    pub init_spec: Option<JobInitSpec>,
}

/// How a job's host requirements meet the fleet right now.
///
/// Counts cover only the hosts the job's owner may `start` on, the same set
/// the scheduler considers, so a caller cannot probe hosts it has no access to
/// by submitting expressions and reading counts back.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostRequirementsReport {
    /// Hosts the owner may start jobs on at all. A zero here is a permissions
    /// problem, not a query problem.
    pub authorized: u32,
    /// Of those, how many the predicate admits.
    pub predicate_matched: u32,
    /// Of those, how many carry an admissible image-set member. Evaluated over
    /// the whole authorized set rather than only the predicate's matches, so
    /// the two failure modes stay distinguishable. Null when the request named
    /// no image set (a concrete image places no constraint on the host).
    pub image_matched: Option<u32>,
    /// Hosts admitted by both: the ones that could actually run the job.
    pub schedulable: u32,
    /// Hosts whose evaluation errored, which counts as not matching. A
    /// forgotten `has()` guard otherwise looks exactly like an empty fleet, so
    /// these are surfaced rather than folded into the miss count.
    pub errored: u32,
    /// The first few evaluation errors, for diagnosis; `errored` is the total.
    pub errors: Vec<HostPredicateError>,
    /// Set when the predicate does not compile, in which case nothing was
    /// evaluated and every match count is zero.
    pub compile_error: Option<String>,
}

/// One host the predicate could not be evaluated against.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostPredicateError {
    pub host_id: Uuid,
    /// The host's name, so a report reads without a second lookup.
    pub name: String,
    /// The evaluator's own diagnostic, e.g. `no such key: model`.
    pub message: String,
}
