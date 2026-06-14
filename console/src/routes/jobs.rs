//! Job pages: the `/jobs` listing, the `/jobs/{id}` overview (with a Terminate
//! button and a placeholder for the eventual live console), and the
//! `/jobs/{id}/terminate` action.
//!
//! All render live from the typed switchboard client and, per the console's
//! convention, the overview ends with the head of the job's audit log.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use maud::{Markup, html};
use serde::Deserialize;
use treadmill_rs::api::switchboard::jobs::{JobInfo, JobParameterView, JobSummary};
use uuid::Uuid;

use crate::serve::AppState;
use crate::session::Session;
use crate::views::audit::audit_feed;
use crate::views::jobs::{image_ref, job_state_badge};
use crate::views::{PageError, layout, timestamp};

/// Query parameters for `GET /jobs` — the opaque keyset cursor for the next
/// page.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    cursor: Option<String>,
}

/// `GET /jobs` — a keyset-paginated listing of the jobs the caller may read.
pub async fn list(
    State(_state): State<AppState>,
    session: Session,
    Query(params): Query<ListParams>,
) -> Result<Markup, PageError> {
    let page = session
        .client
        .list_jobs(None, params.cursor.as_deref())
        .await?;
    let viewer = session.client.whoami().await.ok();
    let caller = viewer.as_ref().map(|w| w.user_id);
    let username = viewer.as_ref().map(|w| w.username.clone());

    Ok(layout(
        "Jobs",
        username.as_deref(),
        html! {
            div.page-head {
                h1 { "Jobs" }
                a.button href="/jobs/new" { "Dispatch a job" }
            }
            section.card {
                @if page.jobs.is_empty() {
                    p.empty { "No jobs to show." }
                } @else {
                    table {
                        thead {
                            tr {
                                th { "State" }
                                th { "Based on" }
                                th { "Owner" }
                                th { "Queued" }
                                th { "Outcome" }
                            }
                        }
                        tbody {
                            @for job in &page.jobs {
                                (job_row(job, caller))
                            }
                        }
                    }
                }
            }
            @if let Some(cursor) = &page.next_cursor {
                p { a href=(format!("/jobs?cursor={cursor}")) { "Next page →" } }
            }
        },
    ))
}

/// One row of the jobs table.
fn job_row(job: &JobSummary, caller: Option<Uuid>) -> Markup {
    html! {
        tr {
            td { (job_state_badge(job.state)) }
            td { a href=(format!("/jobs/{}", job.job_id)) { (image_ref(&job.image)) } }
            td { (owner_cell(job.owner_id, caller)) }
            td { (timestamp(job.queued_at)) }
            td {
                @match &job.termination_reason {
                    Some(reason) => (reason.to_string()),
                    None => span.muted { "—" },
                }
            }
        }
    }
}

/// Render a subject id as `you` when it is the caller, a plain id otherwise, or
/// a dash when the job is orphaned. The owner may be a *group*, so this never
/// links to `/users/{id}`.
fn owner_cell(owner_id: Option<Uuid>, caller: Option<Uuid>) -> Markup {
    match owner_id {
        Some(id) if Some(id) == caller => html! { "you" },
        Some(id) => html! { code { (id) } },
        None => html! { span.muted { "—" } },
    }
}

/// `GET /jobs/{id}` — one job's full overview.
pub async fn show(
    State(_state): State<AppState>,
    session: Session,
    Path(job_id): Path<Uuid>,
) -> Result<Markup, PageError> {
    let job = session.client.get_job(job_id).await?;
    let events = session.client.job_events(job_id).await?.events;
    let viewer = session.client.whoami().await.ok();
    let caller = viewer.as_ref().map(|w| w.user_id);
    let username = viewer.as_ref().map(|w| w.username.clone());

    let finalized = matches!(
        job.state,
        treadmill_rs::api::switchboard::JobState::Finalized
    );

    Ok(layout(
        "Job",
        username.as_deref(),
        html! {
            div.page-head {
                h1 { "Job " code { (job.job_id) } }
                @if !finalized {
                    form method="post" action=(format!("/jobs/{}/terminate", job.job_id)) {
                        button.button.danger type="submit" { "Terminate" }
                    }
                }
            }
            (details_card(&job, caller))
            (console_placeholder())
            (audit_feed(&events))
        },
    ))
}

/// The job's detail card.
fn details_card(job: &JobInfo, caller: Option<Uuid>) -> Markup {
    html! {
        section.card {
            dl.fields {
                dt { "State" }
                dd {
                    (job_state_badge(job.state))
                    @if let Some(stage) = &job.initializing_stage {
                        " " span.muted { (initializing_stage_label(stage)) }
                    }
                }
                dt { "Based on" }
                dd { (image_ref(&job.image)) }
                @if let Some(digest) = &job.resolved_image_digest {
                    dt { "Resolved image" }
                    dd { code title=(digest.to_string()) {
                        (crate::views::jobs::short_digest(digest))
                    } }
                }
                dt { "Owner" }
                dd { (owner_cell(job.owner_id, caller)) }
                dt { "Host" }
                dd { @match job.dispatched_on_host_id {
                    Some(id) => code { (id) },
                    None => span.muted { "—" },
                } }
                @if let Some(endpoints) = &job.ssh_endpoints {
                    dt { "SSH" }
                    dd { @for ep in endpoints {
                        code { (ep.ssh_host) ":" (ep.ssh_port) } " "
                    } }
                }
                dt { "SSH keys" }
                dd { @if job.ssh_keys.is_empty() {
                    span.muted { "—" }
                } @else {
                    (job.ssh_keys.len()) " key(s)"
                } }
                dt { "Restart policy" }
                dd { (job.restart_policy.remaining_restart_count) " remaining" }
                dt { "Host tags" }
                dd { (tag_list(&job.host_tag_requirements)) }
                dt { "Targets" }
                dd { (target_list(&job.target_requirements)) }
                dt { "Parameters" }
                dd { (parameters_view(&job.parameters)) }
                dt { "Timeout" }
                dd { (job.timeout_secs) "s" }
                dt { "Queued" }
                dd { (timestamp(job.queued_at)) }
                @if let Some(t) = job.started_at {
                    dt { "Started" }
                    dd { (timestamp(t)) }
                }
                @if let Some(t) = job.terminated_at {
                    dt { "Terminated" }
                    dd { (timestamp(t)) }
                }
                @if let Some(reason) = &job.termination_reason {
                    dt { "Termination" }
                    dd { (reason.to_string()) }
                }
                @if let Some(status) = &job.task_exit_status {
                    dt { "Workload" }
                    dd { (task_exit_label(status)) }
                }
                @if let Some(msg) = &job.exit_message {
                    dt { "Exit message" }
                    dd { (msg) }
                }
                dt { "Last updated" }
                dd { (timestamp(job.last_updated_at)) }
            }
        }
    }
}

/// Render an opaque tag list as a row of pills, or a dash when empty.
fn tag_list(tags: &[String]) -> Markup {
    html! {
        @if tags.is_empty() {
            span.muted { "—" }
        } @else {
            @for tag in tags {
                span.tag { (tag) } " "
            }
        }
    }
}

/// Render the target (DUT) requirements: one line per requested target, its tag
/// set as pills.
fn target_list(targets: &[Vec<String>]) -> Markup {
    html! {
        @if targets.is_empty() {
            span.muted { "—" }
        } @else {
            @for (i, tags) in targets.iter().enumerate() {
                div { "#" (i) " " (tag_list(tags)) }
            }
        }
    }
}

/// Render the parameters map (sorted by key); secret values are shown redacted.
fn parameters_view(parameters: &std::collections::HashMap<String, JobParameterView>) -> Markup {
    let sorted: BTreeMap<&String, &JobParameterView> = parameters.iter().collect();
    html! {
        @if sorted.is_empty() {
            span.muted { "—" }
        } @else {
            @for (key, p) in sorted {
                div {
                    code { (key) } " = "
                    @if p.secret {
                        span.muted { "(secret)" }
                    } @else {
                        @match &p.value {
                            Some(v) => code { (v) },
                            None => span.muted { "—" },
                        }
                    }
                }
            }
        }
    }
}

/// The static placeholder for the eventual live console (the nats.ws + xterm.js
/// read client is the next phase of the log-streaming work; nothing is wired
/// here yet).
fn console_placeholder() -> Markup {
    html! {
        h2 { "Console output" }
        section.card.console-placeholder {
            p.muted { "Live console streaming is not yet wired up." }
        }
    }
}

fn initializing_stage_label(
    stage: &treadmill_rs::api::switchboard_supervisor::JobInitializingStage,
) -> &'static str {
    use treadmill_rs::api::switchboard_supervisor::JobInitializingStage as S;
    match stage {
        S::Starting => "starting",
        S::FetchingImage => "fetching image",
        S::Allocating => "allocating",
        S::Provisioning => "provisioning",
        S::Booting => "booting",
    }
}

fn task_exit_label(
    status: &treadmill_rs::api::switchboard_supervisor::TaskExitStatus,
) -> &'static str {
    use treadmill_rs::api::switchboard_supervisor::TaskExitStatus as T;
    match status {
        T::Pending => "pending",
        T::Success => "success",
        T::Failure => "failure",
    }
}

/// `POST /jobs/{id}/terminate` — request termination, then return to the job's
/// page. ("Terminate" is the console's verb for the API's `DELETE /jobs/{id}`.)
pub async fn terminate(
    State(_state): State<AppState>,
    session: Session,
    Path(job_id): Path<Uuid>,
) -> Result<Redirect, PageError> {
    session.client.terminate_job(job_id).await?;
    Ok(Redirect::to(&format!("/jobs/{job_id}")))
}
