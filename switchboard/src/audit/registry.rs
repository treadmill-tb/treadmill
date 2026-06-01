//! View-time renderer registry.
//!
//! An audit row's [`payload`](super::persist::StoredEvent::payload) is raw
//! `jsonb`; it has no user-facing representation on its own. Rendering --
//! turning that payload into a string (and, eventually, structured fields) a
//! viewer can read -- is deferred to the moment the row is fetched, so the
//! representation can react to the viewer (redaction, localization) and to
//! product changes without a schema migration.
//!
//! Each event type registers a [`RenderFn`] keyed by its
//! [`event_type`](super::model::AuditEvent::event_type) string into a
//! [`linkme::distributed_slice`]; the read API calls [`render`] which dispatches
//! through that table. The point of link-time registration (rather than a
//! central `match`) is that adding a new event in `define_event!` makes it
//! renderable just by being linked into the binary -- there is no central site
//! that must be edited in lockstep with the event type list.

use linkme::distributed_slice;
use uuid::Uuid;

/// The render fn signature for an event type. Takes the stored `jsonb`
/// payload and the viewer context, returns the viewer-facing message. The
/// renderer is expected to be infallible for any payload it itself produced;
/// a `Result` is returned so a payload that has been corrupted on disk (or
/// pre-dates the current code's understanding of the type's shape) can be
/// surfaced as a render error rather than panicking the read path.
pub type RenderFn = fn(payload: &serde_json::Value, viewer: &ViewerCtx) -> RenderResult;

/// A single entry in the registry: the event type discriminant plus its
/// renderer. `define_event!` populates one of these per event type.
pub struct RendererEntry {
    pub event_type: &'static str,
    pub render: RenderFn,
}

/// The link-time slice every event type registers itself into. New entries are
/// added with `#[distributed_slice(RENDERERS)] static FOO: RendererEntry =
/// ...;` -- handled by the `define_event!` macro in phase 4.
#[distributed_slice]
pub static RENDERERS: [RendererEntry];

/// Per-viewer state a renderer may consult to decide what to redact or how to
/// format. Carries the viewer's subject id and whether they hold global
/// authority (admins-group membership); both are computed once per request by
/// the read API and threaded through to every row's render call.
#[derive(Debug, Clone, Copy)]
pub struct ViewerCtx {
    pub viewer_id: Uuid,
    /// True iff the viewer is a member of the global-authority `admins` group.
    /// Renderers may use this to expose internal fields that are hidden from
    /// regular viewers.
    pub global_authority: bool,
}

/// A rendered event ready to be returned over the read API.
#[derive(Debug, Clone)]
pub struct RenderedEvent {
    /// Viewer-facing message string -- the result of the event type's render
    /// template, with field substitutions and any viewer-dependent redactions
    /// applied.
    pub message: String,
}

/// Outcome of a render attempt.
#[derive(Debug)]
pub enum RenderResult {
    /// Successfully turned the payload into a viewer-facing event.
    Ok(RenderedEvent),
    /// The payload did not match the renderer's expected shape (corrupted row,
    /// or a payload written by an older code version whose type was removed).
    /// The read API logs and skips these.
    PayloadMismatch(serde_json::Error),
}

/// Errors from [`render`]: looking up a renderer for an unknown event type, or
/// the renderer rejecting the payload.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no renderer registered for event type `{0}`")]
    UnknownEventType(String),
    #[error("payload does not match renderer for event type `{event_type}`: {source}")]
    PayloadMismatch {
        event_type: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Look up the renderer for `event_type` and apply it to `payload`.
///
/// Returns [`RenderError::UnknownEventType`] when no event type by that name is
/// linked into this binary -- a real possibility when a stored row pre-dates a
/// code version that has dropped the type (per plan §4, retaining old versions
/// is the recommended discipline to avoid this).
pub fn render(
    event_type: &str,
    payload: &serde_json::Value,
    viewer: &ViewerCtx,
) -> Result<RenderedEvent, RenderError> {
    let entry = RENDERERS
        .iter()
        .find(|e| e.event_type == event_type)
        .ok_or_else(|| RenderError::UnknownEventType(event_type.to_owned()))?;

    match (entry.render)(payload, viewer) {
        RenderResult::Ok(rendered) => Ok(rendered),
        RenderResult::PayloadMismatch(source) => Err(RenderError::PayloadMismatch {
            event_type: event_type.to_owned(),
            source,
        }),
    }
}
