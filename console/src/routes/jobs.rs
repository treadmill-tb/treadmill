//! Job pages: the `/jobs` listing, the `/jobs/{id}` overview (with a Terminate
//! button and a placeholder for the eventual live console), and the
//! `/jobs/{id}/terminate` action.
//!
//! All render live from the typed switchboard client and, per the console's
//! convention, the overview ends with the head of the job's audit log.

use std::collections::{BTreeMap, HashMap};

use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::Deserialize;
use treadmill_rs::api::switchboard::client::ClientError;
use treadmill_rs::api::switchboard::jobs::{
    JobInfo, JobParameter, JobParameterView, JobSummary, RestartPolicy,
};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest};
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
                dd { (job.restart_policy.remaining_restarts) " remaining" }
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
    stage: &treadmill_rs::api::switchboard::jobs::JobInitializingStage,
) -> &'static str {
    use treadmill_rs::api::switchboard::jobs::JobInitializingStage as S;
    match stage {
        S::Starting => "starting",
        S::FetchingImage => "fetching image",
        S::Allocating => "allocating",
        S::Provisioning => "provisioning",
        S::Booting => "booting",
    }
}

fn task_exit_label(status: &treadmill_rs::api::switchboard::jobs::TaskExitStatus) -> &'static str {
    use treadmill_rs::api::switchboard::jobs::TaskExitStatus as T;
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

// ---------------------------------------------------------------------------
// Dispatch form (`/jobs/new`).
//
// The console carries no JavaScript, so the repeated/nested fields use a
// *submit-to-add-rows* model: the form POSTs back to itself, a `action` field
// (set by the clicked submit button) names the intent, and only the `dispatch`
// action actually enqueues a job. Every other action mutates the in-flight row
// set and re-renders, preserving what the user has typed. Parsing reads the raw
// key/value pairs (`Form<Vec<(String, String)>>`) because the field names are
// indexed (`ssh_key[0]`, `target[1][0]`, …), which serde's typed `Form` can't
// express.
// ---------------------------------------------------------------------------

/// One parameter row in the dispatch form.
#[derive(Default, Clone)]
struct ParamRow {
    key: String,
    value: String,
    secret: bool,
}

/// The full in-flight contents of the dispatch form, round-tripped across
/// submit-to-add-rows posts.
#[derive(Default)]
struct FormState {
    /// Selected image-group index digest (string form), or empty if none.
    image_group: String,
    /// Owner selector: `"self"` (or empty) means the caller; otherwise a group
    /// subject id.
    owner: String,
    /// Raw restart-count input (parsed on dispatch).
    restart_count: String,
    /// Raw timeout-seconds input (empty = deployment default).
    timeout_secs: String,
    /// Selected host id from the (currently inert) host dropdown.
    host_select: String,
    ssh_keys: Vec<String>,
    host_tags: Vec<String>,
    params: Vec<ParamRow>,
    targets: Vec<Vec<String>>,
    /// The submit button that was clicked.
    action: String,
}

/// Remove element `i` from `v` if the index is present and in range.
fn remove_at<T>(v: &mut Vec<T>, i: Option<usize>) {
    if let Some(i) = i
        && i < v.len()
    {
        v.remove(i);
    }
}

/// Parse `prefix[N]` → `N`.
fn idx1(name: &str, prefix: &str) -> Option<usize> {
    let rest = name.strip_prefix(prefix)?;
    rest.strip_prefix('[')?.strip_suffix(']')?.parse().ok()
}

/// Parse `prefix[N][M]` → `(N, M)`.
fn idx2(name: &str, prefix: &str) -> Option<(usize, usize)> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('[')?;
    let (a, rest) = rest.split_once(']')?;
    let b = rest.strip_prefix('[')?.strip_suffix(']')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

impl FormState {
    /// Rebuild the form state from the raw posted pairs. Indexed fields are
    /// collected in index order (gaps tolerated) and re-emitted contiguously by
    /// the renderer, so a removed row simply disappears.
    fn from_pairs(pairs: &[(String, String)]) -> Self {
        let mut form = FormState::default();
        let mut ssh: BTreeMap<usize, String> = BTreeMap::new();
        let mut tags: BTreeMap<usize, String> = BTreeMap::new();
        let mut params: BTreeMap<usize, (String, String, bool)> = BTreeMap::new();
        let mut targets: BTreeMap<usize, BTreeMap<usize, String>> = BTreeMap::new();

        for (name, value) in pairs {
            match name.as_str() {
                "image_group" => form.image_group = value.clone(),
                "owner" => form.owner = value.clone(),
                "restart_count" => form.restart_count = value.clone(),
                "timeout_secs" => form.timeout_secs = value.clone(),
                "host_select" => form.host_select = value.clone(),
                "action" => form.action = value.clone(),
                _ => {
                    if let Some(i) = idx1(name, "ssh_key") {
                        ssh.insert(i, value.clone());
                    } else if let Some(i) = idx1(name, "host_tag") {
                        tags.insert(i, value.clone());
                    } else if let Some(i) = idx1(name, "param_key") {
                        params.entry(i).or_default().0 = value.clone();
                    } else if let Some(i) = idx1(name, "param_value") {
                        params.entry(i).or_default().1 = value.clone();
                    } else if let Some(i) = idx1(name, "param_secret") {
                        params.entry(i).or_default().2 = true;
                    } else if let Some((i, j)) = idx2(name, "target") {
                        targets.entry(i).or_default().insert(j, value.clone());
                    }
                }
            }
        }

        form.ssh_keys = ssh.into_values().collect();
        form.host_tags = tags.into_values().collect();
        form.params = params
            .into_values()
            .map(|(key, value, secret)| ParamRow { key, value, secret })
            .collect();
        form.targets = targets
            .into_values()
            .map(|tags| tags.into_values().collect())
            .collect();
        form
    }

    /// Apply a row-editing action (anything but `dispatch`).
    fn apply_action(&mut self) {
        let mut parts = self.action.split(':');
        let verb = parts.next().unwrap_or("");
        let a = parts.next().and_then(|s| s.parse::<usize>().ok());
        let b = parts.next().and_then(|s| s.parse::<usize>().ok());
        match verb {
            "add_ssh_key" => self.ssh_keys.push(String::new()),
            "remove_ssh_key" => remove_at(&mut self.ssh_keys, a),
            "add_host_tag" => self.host_tags.push(String::new()),
            "remove_host_tag" => remove_at(&mut self.host_tags, a),
            "add_param" => self.params.push(ParamRow::default()),
            "remove_param" => remove_at(&mut self.params, a),
            "add_target" => self.targets.push(vec![String::new()]),
            "remove_target" => remove_at(&mut self.targets, a),
            "add_target_tag" => {
                if let Some(i) = a
                    && let Some(t) = self.targets.get_mut(i)
                {
                    t.push(String::new());
                }
            }
            "remove_target_tag" => {
                if let (Some(i), Some(j)) = (a, b)
                    && let Some(t) = self.targets.get_mut(i)
                    && j < t.len()
                {
                    t.remove(j);
                }
            }
            _ => {}
        }
    }

    /// Assemble a [`JobRequest`] from the form, or return a user-facing error
    /// message. Empty rows are dropped; an empty target (no tags) is dropped.
    fn build_request(&self) -> Result<JobRequest, String> {
        let image_group = Uuid::parse_str(self.image_group.trim())
            .map_err(|_| "Select an image group.".to_string())?;

        let owner = if self.owner.is_empty() || self.owner == "self" {
            None
        } else {
            Some(Uuid::parse_str(&self.owner).map_err(|_| "Invalid owner selection.".to_string())?)
        };

        let restart = self.restart_count.trim();
        let max_restarts = if restart.is_empty() {
            0
        } else {
            restart
                .parse()
                .map_err(|_| "Restart count must be a non-negative integer.".to_string())?
        };

        let override_timeout = {
            let t = self.timeout_secs.trim();
            if t.is_empty() {
                None
            } else {
                let secs: i64 = t
                    .parse()
                    .map_err(|_| "Timeout must be a whole number of seconds.".to_string())?;
                Some(chrono::Duration::seconds(secs))
            }
        };

        let nonempty = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };

        let parameters: HashMap<String, JobParameter> = self
            .params
            .iter()
            .filter(|p| !p.key.trim().is_empty())
            .map(|p| {
                (
                    p.key.trim().to_string(),
                    JobParameter {
                        value: p.value.clone(),
                        secret: p.secret,
                    },
                )
            })
            .collect();

        let target_requirements: Vec<Vec<String>> = self
            .targets
            .iter()
            .map(|t| nonempty(t))
            .filter(|t| !t.is_empty())
            .collect();

        Ok(JobRequest {
            init_spec: JobInitSpec::ImageGroup {
                group_id: image_group,
                generation: None,
            },
            owner,
            ssh_keys: nonempty(&self.ssh_keys),
            restart_policy: RestartPolicy { max_restarts },
            parameters,
            host_tag_requirements: nonempty(&self.host_tags),
            target_requirements,
            override_timeout,
        })
    }
}

/// The select-option lists and nav username needed to render the dispatch form.
struct FormOptions {
    username: String,
    /// `(value, label)` for the owner `<select>` (self + the caller's groups).
    owners: Vec<(String, String)>,
    /// `(group-id, label)` for the image-group `<select>`.
    image_groups: Vec<(String, String)>,
    /// `(host-id, label)` for the (inert) host `<select>`.
    hosts: Vec<(String, String)>,
}

/// Fetch everything the dispatch form's selectors need.
async fn load_options(session: &Session) -> Result<FormOptions, PageError> {
    let me = session.client.get_me().await?;
    let groups = session.client.list_image_groups().await?;
    let hosts = session.client.list_hosts().await?;

    let mut owners = vec![("self".to_string(), format!("me ({})", me.profile.username))];
    owners.extend(
        me.groups
            .iter()
            .map(|g| (g.group_id.to_string(), g.name.clone())),
    );

    let image_groups = groups
        .iter()
        .map(|g| {
            let label = g.label.clone().unwrap_or_else(|| g.name.clone());
            (g.id.to_string(), label)
        })
        .collect();

    let hosts = hosts
        .iter()
        .map(|h| {
            let label = if h.live {
                h.name.clone()
            } else {
                format!("{} (offline)", h.name)
            };
            (h.host_id.to_string(), label)
        })
        .collect();

    Ok(FormOptions {
        username: me.profile.username,
        owners,
        image_groups,
        hosts,
    })
}

/// `GET /jobs/new` — render the empty dispatch form.
pub async fn new_form(
    State(_state): State<AppState>,
    session: Session,
) -> Result<Markup, PageError> {
    let opts = load_options(&session).await?;
    Ok(render_dispatch(&opts, &FormState::default(), None))
}

/// `POST /jobs/new` — either apply a row-editing action and re-render, or (on
/// `dispatch`) enqueue the job and redirect to it.
pub async fn dispatch(
    State(_state): State<AppState>,
    session: Session,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, PageError> {
    let mut form = FormState::from_pairs(&pairs);

    if form.action != "dispatch" {
        form.apply_action();
        let opts = load_options(&session).await?;
        return Ok(render_dispatch(&opts, &form, None).into_response());
    }

    match form.build_request() {
        Ok(req) => match session.client.enqueue_job(&req).await {
            Ok(resp) => Ok(Redirect::to(&format!("/jobs/{}", resp.job_id)).into_response()),
            Err(ClientError::Unauthorized) => Err(PageError::Unauthorized),
            Err(e) => {
                let opts = load_options(&session).await?;
                Ok(render_dispatch(&opts, &form, Some(e.to_string())).into_response())
            }
        },
        Err(msg) => {
            let opts = load_options(&session).await?;
            Ok(render_dispatch(&opts, &form, Some(msg)).into_response())
        }
    }
}

/// Render the dispatch form. `error`, when present, is shown as a banner above
/// the form, which keeps everything the user has entered.
fn render_dispatch(opts: &FormOptions, form: &FormState, error: Option<String>) -> Markup {
    layout(
        "Dispatch a job",
        Some(&opts.username),
        html! {
            h1 { "Dispatch a job" }
            @if let Some(msg) = &error {
                p.form-error { (msg) }
            }
            form method="post" action="/jobs/new" {
                section.card {
                    div.form-field {
                        label for="image_group" { "Image group" }
                        select #image_group name="image_group" required {
                            option value="" disabled selected[form.image_group.is_empty()] { "— select —" }
                            @for (value, label) in &opts.image_groups {
                                option value=(value) selected[*value == form.image_group] { (label) }
                            }
                        }
                    }
                    div.form-field {
                        label for="owner" { "Owner" }
                        select #owner name="owner" {
                            @for (value, label) in &opts.owners {
                                option value=(value) selected[*value == form.owner] { (label) }
                            }
                        }
                    }
                    div.form-field {
                        label for="restart_count" { "Restarts" }
                        input #restart_count type="number" min="0" name="restart_count"
                            value=(if form.restart_count.is_empty() { "0" } else { &form.restart_count });
                    }
                    div.form-field {
                        label for="timeout_secs" { "Timeout (seconds)" }
                        input #timeout_secs type="number" min="1" name="timeout_secs"
                            placeholder="deployment default" value=(form.timeout_secs);
                    }
                    div.form-field {
                        label for="host_select" { "Host" }
                        select #host_select name="host_select" {
                            option value="" selected[form.host_select.is_empty()] { "— any matching host —" }
                            @for (value, label) in &opts.hosts {
                                option value=(value) selected[*value == form.host_select] { (label) }
                            }
                        }
                        // TODO(console): the host dropdown is currently inert —
                        // its value is ignored and derives no tag requirements.
                        // Wiring it requires a host-pinning semantics decision
                        // (likely a reserved tml.host/id=<uuid> tag requirement).
                        p.muted { "Note: host selection is not yet wired up; jobs match on the tags below." }
                    }
                }

                (row_section("SSH keys", "ssh_key", &form.ssh_keys))
                (row_section("Host tags", "host_tag", &form.host_tags))
                (params_section(&form.params))
                (targets_section(&form.targets))

                div.form-actions {
                    button.button type="submit" name="action" value="dispatch" { "Dispatch" }
                    a href="/jobs" { "Cancel" }
                }
            }
        },
    )
}

/// A simple list-of-text-inputs section with add/remove buttons (used for SSH
/// keys and host tags). `field` is the input-name prefix (`ssh_key`,
/// `host_tag`).
fn row_section(title: &str, field: &str, rows: &[String]) -> Markup {
    html! {
        h2 { (title) }
        section.card {
            @if rows.is_empty() {
                p.empty { "None." }
            }
            @for (i, value) in rows.iter().enumerate() {
                div.row {
                    input type="text" name=(format!("{field}[{i}]")) value=(value);
                    button.button.row-remove type="submit" name="action"
                        value=(format!("remove_{field}:{i}")) { "Remove" }
                }
            }
            button.button.row-add type="submit" name="action"
                value=(format!("add_{field}")) { "Add" }
        }
    }
}

/// The parameters section: key / value / secret rows.
fn params_section(rows: &[ParamRow]) -> Markup {
    html! {
        h2 { "Parameters" }
        section.card {
            @if rows.is_empty() {
                p.empty { "None." }
            }
            @for (i, p) in rows.iter().enumerate() {
                div.row {
                    input type="text" name=(format!("param_key[{i}]")) placeholder="key" value=(p.key);
                    input type="text" name=(format!("param_value[{i}]")) placeholder="value" value=(p.value);
                    label.secret-toggle {
                        input type="checkbox" name=(format!("param_secret[{i}]")) checked[p.secret];
                        " secret"
                    }
                    button.button.row-remove type="submit" name="action"
                        value=(format!("remove_param:{i}")) { "Remove" }
                }
            }
            button.button.row-add type="submit" name="action" value="add_param" { "Add parameter" }
        }
    }
}

/// The target (DUT) requirements section: a list of targets, each a nested
/// list of tag inputs.
fn targets_section(targets: &[Vec<String>]) -> Markup {
    html! {
        h2 { "Targets (DUTs)" }
        section.card {
            @if targets.is_empty() {
                p.empty { "None." }
            }
            @for (i, tags) in targets.iter().enumerate() {
                div.target {
                    div.target-head {
                        strong { "Target #" (i) }
                        button.button.row-remove type="submit" name="action"
                            value=(format!("remove_target:{i}")) { "Remove target" }
                    }
                    @for (j, tag) in tags.iter().enumerate() {
                        div.row {
                            input type="text" name=(format!("target[{i}][{j}]")) placeholder="tag" value=(tag);
                            button.button.row-remove type="submit" name="action"
                                value=(format!("remove_target_tag:{i}:{j}")) { "Remove" }
                        }
                    }
                    button.button.row-add type="submit" name="action"
                        value=(format!("add_target_tag:{i}")) { "Add tag" }
                }
            }
            button.button.row-add type="submit" name="action" value="add_target" { "Add target" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid image-group id for tests.
    const GROUP_ID: &str = "00000000-0000-0000-0000-0000000000ab";

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn from_pairs_collects_indexed_and_nested_fields() {
        let pairs = vec![
            pair("image_group", GROUP_ID),
            pair("owner", "self"),
            pair("ssh_key[0]", "key-a"),
            pair("ssh_key[1]", "key-b"),
            pair("host_tag[0]", "arch=arm64"),
            pair("param_key[0]", "FOO"),
            pair("param_value[0]", "bar"),
            pair("param_key[1]", "TOKEN"),
            pair("param_value[1]", "s3cr3t"),
            pair("param_secret[1]", "on"),
            pair("target[0][0]", "dut=nrf52"),
            pair("target[0][1]", "role=primary"),
            pair("target[1][0]", "dut=lpc"),
            pair("action", "dispatch"),
        ];
        let form = FormState::from_pairs(&pairs);

        assert_eq!(form.image_group, GROUP_ID);
        assert_eq!(form.ssh_keys, vec!["key-a", "key-b"]);
        assert_eq!(form.host_tags, vec!["arch=arm64"]);
        assert_eq!(form.params.len(), 2);
        assert_eq!(form.params[0].key, "FOO");
        assert!(!form.params[0].secret);
        assert_eq!(form.params[1].key, "TOKEN");
        assert!(form.params[1].secret);
        assert_eq!(
            form.targets,
            vec![vec!["dut=nrf52", "role=primary"], vec!["dut=lpc"]]
        );
        assert_eq!(form.action, "dispatch");
    }

    #[test]
    fn apply_action_adds_and_removes_rows() {
        let mut form = FormState {
            ssh_keys: vec!["a".into(), "b".into(), "c".into()],
            ..FormState::default()
        };
        form.action = "remove_ssh_key:1".into();
        form.apply_action();
        assert_eq!(form.ssh_keys, vec!["a", "c"]);

        form.action = "add_target".into();
        form.apply_action();
        assert_eq!(form.targets, vec![vec![String::new()]]);

        form.action = "add_target_tag:0".into();
        form.apply_action();
        assert_eq!(form.targets[0].len(), 2);

        form.action = "remove_target_tag:0:0".into();
        form.apply_action();
        assert_eq!(form.targets[0].len(), 1);
    }

    #[test]
    fn build_request_drops_empties_and_maps_fields() {
        let form = FormState {
            image_group: GROUP_ID.into(),
            owner: "self".into(),
            restart_count: "2".into(),
            timeout_secs: "  ".into(),
            ssh_keys: vec!["key-a".into(), "  ".into()],
            host_tags: vec!["arch=arm64".into(), "".into()],
            params: vec![
                ParamRow {
                    key: "FOO".into(),
                    value: "bar".into(),
                    secret: false,
                },
                ParamRow {
                    key: "  ".into(),
                    value: "ignored".into(),
                    secret: false,
                },
            ],
            targets: vec![vec!["dut=nrf52".into(), "".into()], vec!["".into()]],
            ..FormState::default()
        };

        let req = form.build_request().expect("valid request");
        assert!(matches!(req.init_spec, JobInitSpec::ImageGroup { .. }));
        assert_eq!(req.owner, None);
        assert_eq!(req.restart_policy.max_restarts, 2);
        assert_eq!(req.override_timeout, None);
        assert_eq!(req.ssh_keys, vec!["key-a"]);
        assert_eq!(req.host_tag_requirements, vec!["arch=arm64"]);
        assert_eq!(req.parameters.len(), 1);
        assert!(req.parameters.contains_key("FOO"));
        // The all-empty second target is dropped; the first keeps its one tag.
        assert_eq!(req.target_requirements, vec![vec!["dut=nrf52".to_string()]]);
    }

    #[test]
    fn build_request_requires_an_image_group() {
        let form = FormState::default();
        assert!(form.build_request().is_err());
    }

    #[test]
    fn render_dispatch_emits_indexed_field_names_and_selection() {
        let opts = FormOptions {
            username: "alice".into(),
            owners: vec![
                ("self".into(), "me (alice)".into()),
                ("group-1".into(), "my-team".into()),
            ],
            image_groups: vec![(GROUP_ID.into(), "my-group".into())],
            hosts: vec![("host-1".into(), "rpi-lab-03".into())],
        };
        let form = FormState {
            image_group: GROUP_ID.into(),
            ssh_keys: vec!["key-a".into()],
            targets: vec![vec!["dut=nrf52".into()]],
            ..FormState::default()
        };
        let html = render_dispatch(&opts, &form, Some("boom".into())).into_string();

        // The error banner and the indexed/nested field names are present.
        assert!(html.contains("boom"));
        assert!(html.contains(r#"name="ssh_key[0]""#));
        assert!(html.contains(r#"name="target[0][0]""#));
        assert!(html.contains(r#"value="add_target_tag:0""#));
        // The selected image group is marked selected.
        assert!(html.contains("selected"));
        // The inert host dropdown is rendered with its note.
        assert!(html.contains("host selection is not yet wired up"));
    }

    #[test]
    fn build_request_parses_timeout_and_group_owner() {
        let owner = Uuid::new_v4();
        let form = FormState {
            image_group: GROUP_ID.into(),
            owner: owner.to_string(),
            timeout_secs: "3600".into(),
            ..FormState::default()
        };
        let req = form.build_request().expect("valid request");
        assert_eq!(req.owner, Some(owner));
        assert_eq!(req.override_timeout, Some(chrono::Duration::seconds(3600)));
    }
}
