//! Shared rendering helpers for the job pages (listing + overview).
//!
//! These render the small pieces both the `/jobs` table and the `/jobs/{id}`
//! detail page show identically — a job-state badge, the "based on" image
//! reference, and an abbreviated image digest — so the two stay visually
//! consistent.

use maud::{Markup, html};
use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::jobs::JobImageRef;
use treadmill_rs::image::Digest;
use uuid::Uuid;

/// Human label for a [`JobState`].
pub fn job_state_label(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Scheduled => "scheduled",
        JobState::Initializing => "initializing",
        JobState::Ready => "ready",
        JobState::Terminating => "terminating",
        JobState::Finalized => "finalized",
    }
}

/// A job-state badge: the `.tag` pill with a per-state modifier class so a
/// stylesheet can color it.
pub fn job_state_badge(state: JobState) -> Markup {
    let class = match state {
        JobState::Queued | JobState::Scheduled => "state-pending",
        JobState::Initializing | JobState::Terminating => "state-busy",
        JobState::Ready => "state-ready",
        JobState::Finalized => "state-done",
    };
    html! { span.tag.(class) { (job_state_label(state)) } }
}

/// Abbreviate an image digest to `sha256:` plus its first 12 hex characters,
/// keeping tables and cards readable while staying recognizable.
pub fn short_digest(digest: &Digest) -> String {
    let full = digest.to_string();
    // `sha256:` (7) + 12 hex = 19; anything shorter is shown whole.
    if full.len() > 19 {
        format!("{}…", &full[..19])
    } else {
        full
    }
}

/// Abbreviate a catalog UUID to its first 8 hex characters for compact display;
/// callers show the full value in a `title` attribute.
fn short_id(id: &Uuid) -> String {
    let full = id.simple().to_string();
    format!("{}…", &full[..8])
}

/// Render what a job is based on, with a link to the referenced job for
/// resume/restart and an abbreviated catalog id (full value in the `title`) for
/// an image or image group; a group also shows its frozen generation.
pub fn image_ref(image: &JobImageRef) -> Markup {
    match image {
        JobImageRef::Image { image_id } => html! {
            code title=(image_id.to_string()) { (short_id(image_id)) }
        },
        JobImageRef::ImageGroup {
            group_id,
            generation,
        } => html! {
            "group " code title=(group_id.to_string()) { (short_id(group_id)) }
            " gen " (generation)
        },
        JobImageRef::Resume { job_id } => html! {
            "resume of " a href=(format!("/jobs/{job_id}")) { code { (job_id) } }
        },
        JobImageRef::Restart { job_id } => html! {
            "restart of " a href=(format!("/jobs/{job_id}")) { code { (job_id) } }
        },
    }
}
