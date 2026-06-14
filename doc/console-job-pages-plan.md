# Console Job Pages — Implementation Plan

**Status:** planned — not yet started
· **Date:** 2026-06-14
· **Branch:** `dev/switchboard-refactor`

> One-time implementation roadmap for the web console's job pages (listing, job
> overview, and a dispatch form), plus the read-only `GET /hosts` endpoint and
> the typed-client methods they depend on. Self-contained so a contributor
> (human or agent) can pick up the work without the design conversation that
> produced it. As with the other plans under `doc/`, the durable specification
> lives in the code once written (Rustdoc + the committed OpenAPI snapshot); this
> file may be deleted once the pages land — hence the `[REMOVE]` commit prefix.

---

## 1. Scope

Extend the currently rudimentary console (`console/`) — today only login + the
`/me` and `/users/{id}` profile pages — with the user-facing job surface:

- **`GET /jobs`** — a paginated listing of the jobs the caller may read.
- **`GET /jobs/{id}`** — a single job's overview: a details card, a **Terminate**
  button, a static placeholder for the eventual live console, and the reusable
  audit-log component.
- **`GET /jobs/new` + `POST /jobs/new`** — a dispatch form exposing every field a
  user can set in `POST /jobs`.

Supporting work this requires:

- A new read-only **`GET /hosts`** switchboard endpoint (the dispatch form's host
  dropdown needs a host list; none exists today).
- New **typed-client** methods on `SwitchboardClient` (it currently has only GET
  helpers and no job/host methods).

### Terminology

The user-facing verb for ending a job is **"terminate"** throughout the console
(button label, page copy, console route name). Note this is *console-only*
wording: the underlying switchboard endpoint remains `DELETE /jobs/{id}` and the
SQL/audit layer keeps its existing `cancel`/`request_cancel`/`JobCanceled`
names — do **not** rename the API or DB layer as part of this work. The console
simply presents "terminate" to the user.

### Explicit non-goals (deferred)

- **Live console output.** The xterm.js + nats.ws live tail is a *static
  placeholder* only here; wiring it is the next phase of the log-streaming plan
  (`doc/log-streaming-plan.md`). No log-token fetch, no NATS, no xterm.
- **Host selection actually doing anything.** The host dropdown is rendered and
  populated, but its value is **ignored** (see §5, step 6) — it derives no tag
  requirements yet. A big `TODO` marks this for a later change; getting the
  front-end right comes first.
- **init_spec variants other than image group.** The dispatch form only offers
  `ImageGroup { image_group: <digest> }`. No concrete-image variant, no raw
  digest fallback, no resume/restart.
- **Any JavaScript.** The console stays zero-JS; every page must be fully
  functional without it (see §3).
- **Host-pinning / exact host targeting.** Out of scope; see §6 for the design
  discussion that led to deferring it.

## 2. Grounding (confirmed by inspection)

- **Console architecture.** `console/` is a server-side `maud`-rendered app with
  **zero JavaScript**, a single embedded classless stylesheet
  (`console/src/assets.rs::STYLE_CSS`), and a session that is just the
  switchboard bearer token in an `HttpOnly` cookie (`console/src/session.rs`).
  The `Session` extractor yields a `SwitchboardClient` bound to that token.
  Pages return `Result<Markup, PageError>`; `PageError` maps a client
  `Unauthorized` to a redirect to `/login` and any other status to a minimal
  error page (`console/src/views/mod.rs`).
- **Reusable components.** `console/src/views/audit.rs::audit_feed(&[RenderedAuditRow])`
  already exists and is dropped in at the bottom of every resource page. The job
  overview page reuses it as-is. `views/mod.rs::timestamp()` and `layout()` are
  the shared chrome.
- **Jobs API (all present).** In `switchboard/src/routes/jobs.rs`:
  - `GET /jobs` → `JobListResponse { jobs: Vec<JobSummary>, next_cursor: Option<String> }`,
    keyset pagination on `(queued_at, job_id)` desc, already filtered to jobs the
    caller may read. Query params: `limit` (clamped 1..=200, default 50),
    `cursor` (opaque).
  - `GET /jobs/{id}` → `JobInfo` (full view; secret params redacted).
  - `POST /jobs` (`JobRequest`) → `201` + `EnqueueJobResponse { job_id }`.
  - `DELETE /jobs/{id}` → `202` (termination initiated) / `204` (already
    finalized).
  - `GET /jobs/{id}/events` → `AuditFeedResponse`.
  - `POST /jobs/{id}/log-token` → `LogStreamCredentials` (not used here; the
    console is a static placeholder).
- **`JobRequest` fields** (`treadmill-rs/src/api/switchboard.rs`):
  `init_spec` (tagged enum: `Image{image}` / `ImageGroup{image_group}` /
  `ResumeJob{job_id}` / `RestartJob{job_id}`), `owner: Option<Uuid>` (caller or a
  group it belongs to), `ssh_keys: Vec<String>`, `restart_policy {
  remaining_restart_count: usize }`, `parameters: HashMap<String, ParameterValue
  { value: String, secret: bool }>`, `host_tag_requirements: Vec<String>`,
  `target_requirements: Vec<Vec<String>>`, `override_timeout: Option<Duration>`
  (serialized via `crate::util::chrono::optional_duration`, schema
  `Option<String>`).
- **Image-group catalog listing exists.** `GET /image-groups` →
  `Vec<ImageGroupInfo { id, index_digest: Digest, owner_id, label, created_at,
  members }>`. The dispatch form's init_spec dropdown is populated from this.
- **No host listing exists.** `switchboard/src/routes/mod.rs` exposes only
  `/hosts/{id}/events` and `/hosts/{id}/connect`. A `GET /hosts` must be added.
- **Hosts/targets schema** (`switchboard/SCHEMA.sql`): `hosts(host_id, name,
  tags text[], last_seen_at, current_job, …)` and `host_targets(host_id, name,
  tags text[], …)`. Host tags are opaque (`tags @> required` is the matcher's
  only operation; "the matcher never parses them").
- **Typed client gap.** `SwitchboardClient`
  (`treadmill-rs/src/api/switchboard/client.rs`) has a private `get_json` helper
  and only GET methods. No POST/DELETE helper, no job/host methods.
- **Owner groups are available.** `GET /users/me` → `SelfUserProfile` carries
  `profile` + `groups: Vec<{ group_id, name, source }>`, enough to build the
  owner dropdown (self + groups) without an extra call.

## 3. No-JavaScript constraint (and its one real cost)

The console must remain usable with **full functionality and no JS**. This is
achievable because the console is a *server-side* app, but it shapes two designs:

- **Repeated / nested inputs use a submit-to-add-rows model.** `JobRequest` has
  several list/map/nested fields (`ssh_keys`, `parameters`,
  `host_tag_requirements`, and `target_requirements`, which is
  `Vec<Vec<String>>`). Without JS there is no client-side "add row". Instead the
  dispatch form **posts back to itself**: a hidden `action` field distinguishes
  row-editing submits from the final dispatch (see §5, step 6). Only the
  `dispatch` action builds a `JobRequest` and calls the API; every other action
  re-renders the form, preserving entered values and mutating the row set. This
  is the most involved part of the work and the price of zero-JS.
- **The Terminate button is a POST form.** HTML forms cannot issue `DELETE`, so a
  console route `POST /jobs/{id}/terminate` bridges to the API's `DELETE
  /jobs/{id}` and redirects back.

If any future field genuinely cannot be expressed without JS, surface it rather
than silently adding a script; small page-specific vanilla snippets are a last
resort only.

## 4. Switchboard + shared-crate changes

### 4.1 `GET /hosts` (read-only)

- **New type** `treadmill-rs/src/api/switchboard/hosts.rs`:
  ```text
  HostInfo {
      host_id: Uuid,
      name: String,
      tags: Vec<String>,
      live: bool,                       // last_seen_at within liveness window
      last_seen_at: Option<DateTime<Utc>>,
      targets: Vec<HostTarget>,
  }
  HostTarget { name: String, tags: Vec<String> }
  ```
  Register the module in `treadmill-rs/src/api/switchboard.rs`.
- **Handler** in `switchboard/src/routes/hosts.rs` + register
  `GET /hosts` in `switchboard/src/routes/mod.rs::api_router`. Returns
  `Json<Vec<HostInfo>>`. Authorization: any authenticated subject may list
  (matches the scheduler's current permissiveness; an authz tightening is a
  separate, already-tracked TODO). No audit emit on a read.
- **SQL** in `switchboard/src/sql/` (e.g. `sql/host.rs`): select hosts and their
  `host_targets`, assembling `targets` per host. The richer endpoint (targets
  included) is built now even though host selection is inert, per the design
  decision.
- **Snapshot/cache chores** (per AGENTS.md §4, §6):
  - Regenerate the OpenAPI snapshot:
    `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard --test openapi_spec`.
  - Regenerate the root `.sqlx` cache from a clean build in the `.#database`
    shell (`cargo clean -p treadmill-switchboard` first), then verify with an
    offline `--all-targets` build.

### 4.2 Typed client methods

In `treadmill-rs/src/api/switchboard/client.rs`, add generic `post_json` (for a
typed request + typed response, treating 2xx as success) and `delete` helpers
mirroring `get_json`'s error mapping (`401` → `Unauthorized`, other non-2xx →
`Status`). Then the methods the console calls:

- `list_jobs(limit: Option<u32>, cursor: Option<&str>) -> JobListResponse`
- `get_job(id: Uuid) -> JobInfo`
- `job_events(id: Uuid) -> AuditFeedResponse`
- `enqueue_job(req: &JobRequest) -> EnqueueJobResponse`
- `terminate_job(id: Uuid) -> ()`  *(calls `DELETE /jobs/{id}`; console-facing
  name uses "terminate", consistent with §1)*
- `list_hosts() -> Vec<HostInfo>`
- `list_image_groups() -> Vec<ImageGroupInfo>`

(`get_me` already exists for the owner dropdown.)

## 5. Console pages

All under `console/src/routes/jobs.rs` (new module, registered in
`console/src/routes/mod.rs`), with view helpers in `console/src/views/jobs.rs`
(new) as needed. Add a **"jobs"** nav link in `views/mod.rs::layout()`.

### Step 1 — `GET /jobs` listing (`jobs::list`)

- Read optional `?cursor=` (and optionally `?limit=`) from the query; call
  `list_jobs`.
- Table columns: **state** (badge, see §7), **image** (short digest for
  image/group; a link to the referenced job for resume/restart), **owner**
  (render `you` when `owner_id == caller`, otherwise `code{uuid}` — `owner_id`
  may be a *group*, so do **not** blindly link to `/users/{id}`), **queued /
  started / terminated** timestamps (reuse `timestamp()`), and **outcome**
  (termination reason + task exit status when finalized).
- Each row links to `/jobs/{id}`.
- **Pagination is forward-only**: when `next_cursor` is `Some`, render a "Next"
  link carrying `?cursor=<next_cursor>`. (No "previous"; keyset pagination is
  one-directional and there is no total count.)

### Step 2 — `GET /jobs/{id}` overview (`jobs::show`)

- Call `get_job` + `job_events`.
- **Details card** (`dl.fields`): state (+ initializing stage), image ref,
  resolved image digest, owner, dispatched-on host, SSH endpoints, ssh keys,
  restart policy, parameters (secret values shown as a redacted marker, never a
  value — they arrive as `None`), timeout, and all timestamps; when finalized,
  termination reason / task exit status / exit message / terminated-at.
- **Terminate button:** a `<form method="post" action="/jobs/{id}/terminate">`
  with a submit button, rendered **only when the job is not finalized**
  (`state != Finalized`).
- **Console placeholder:** a static `section.card` titled e.g. "Console output"
  with muted copy ("live log streaming is not yet wired — see the log-streaming
  plan"). No dynamic behavior.
- **Audit feed:** reuse `audit_feed(&events)` at the bottom.

### Step 3 — `POST /jobs/{id}/terminate` (`jobs::terminate`)

- Call `terminate_job(id)`, then `Redirect::to("/jobs/{id}")`. Map errors via
  `PageError`. Idempotent from the user's view (a `204` is a no-op).

### Step 4 — `GET /jobs/new` + `POST /jobs/new` dispatch form

The dispatch form (`jobs::new_form` for the initial GET, `jobs::dispatch` for all
POSTs) uses the **submit-to-add-rows** model from §3.

**Form state & parsing.** Extract the body as `Form<Vec<(String, String)>>`
(serde_urlencoded preserves repeated keys; axum's typed `Form` would collapse
them). Build the structures manually from indexed field names — e.g.
`ssh_key[i]`, `param_key[i]` / `param_value[i]` / `param_secret[i]`,
`host_tag[i]`, and the two-level `target[i][j]` for `target_requirements`.

**The `action` field** (hidden input set by the clicked submit button)
distinguishes intents:

- `add_ssh_key`, `remove_ssh_key:<i>`
- `add_param`, `remove_param:<i>`
- `add_host_tag`, `remove_host_tag:<i>`
- `add_target`, `remove_target:<i>`
- `add_target_tag:<i>`, `remove_target_tag:<i>:<j>`  *(two-level nesting)*
- `dispatch`

On any action other than `dispatch`, re-render the form preserving all submitted
values and applying the row mutation. On `dispatch`, assemble the `JobRequest`,
call `enqueue_job`, and `Redirect::to("/jobs/{job_id}")` on success (surfacing
validation errors from the API via `PageError`, ideally re-rendering the form
with the error visible).

**Fields exposed:**

- **init_spec** — an image-group `<select>` populated from `list_image_groups`
  (option label: `label` if present else short index digest; value: the index
  digest). Always produces `ImageInitSpec::ImageGroup { image_group }`. The only
  variant offered.
- **owner** — a `<select>` of *self* + the caller's groups (from `get_me` →
  `profile.groups`), default self. Self maps to `owner: None` (API default);
  a group maps to `owner: Some(group_id)`.
- **ssh_keys** — submit-to-add rows of text inputs.
- **restart_policy.remaining_restart_count** — a number input, default `0`.
- **parameters** — submit-to-add rows, each `key` + `value` + a `secret`
  checkbox → `ParameterValue { value, secret }`.
- **host_tag_requirements** — submit-to-add rows of text inputs.
- **target_requirements** — submit-to-add *targets*, each target itself a
  submit-to-add list of tag inputs (the `Vec<Vec<String>>` two-level case).
- **override_timeout** — an optional number input (seconds) → `chrono::Duration`;
  empty means "use the deployment default" (`override_timeout: None`).
- **host `<select>`** — populated from `list_hosts` (label: `name`, optionally a
  liveness marker). **Its value is currently ignored** — it derives no tag
  requirements. Mark with a prominent `TODO` referencing this plan; wiring it
  (and deciding host-pinning semantics, see §6) is a later change.

### Step 5 — routing & nav

Register in `console/src/routes/mod.rs`:

```text
GET  /jobs                  -> jobs::list
GET  /jobs/new              -> jobs::new_form
POST /jobs/new              -> jobs::dispatch
GET  /jobs/{id}             -> jobs::show
POST /jobs/{id}/terminate   -> jobs::terminate
```

Add a "jobs" link to the nav in `layout()`.

## 6. Design discussion: host selection / pinning (deferred)

`JobRequest` has **no host-id field**; the scheduler matches jobs to hosts purely
by tag-superset (`hosts.tags @> host_tag_requirements`). So "select a host" has
no native representation today. Options considered:

1. **Server-side tag resolve** — the console resolves the chosen host's current
   tags into `host_tag_requirements`. Needs only `GET /hosts`, no matcher change,
   but is "run on a host *like* this one", not an exact pin.
2. **Reserved namespace pin** — teach `eligible_hosts` to honor a reserved
   `tml.host/id=<uuid>` requirement *derived from `host_id`* (not stored in
   `tags`, so it can't drift). Exact pin, schema-free, but a real scheduler
   change and still subject to the open authz TODO (jobs aren't yet restricted to
   hosts the caller may use, so a pin is spoofable).
3. **Magic tag in the `tags` column** — rely on a `host=<id>` value living in the
   host's real tags. Breaks the opaque-tag invariant, can drift, spoofable.
   Advised against.

**Decision:** for this change, render the dropdown but **ignore its value**
(stub, big TODO). The endpoint (`GET /hosts`, incl. targets) is still built so a
later change can wire selection without further API work. The pinning-semantics
decision (likely option 2, the reserved namespace, *after* the authz TODO is
closed) is left for that later change.

## 7. Styling

Extend `console/src/assets.rs::STYLE_CSS` (the embedded classless sheet) for:

- form layout (labels, inputs, the add-row / remove-row controls and buttons),
- job-state badges (reuse the existing `.tag`; optionally per-state accent
  colors),
- the static console placeholder panel.

Keep the restrained, classless aesthetic already established.

## 8. Implementation phases (one focused commit each)

Each commit must build, pass `clippy`, and pass its relevant hermetic check on
its own (AGENTS.md §3, §8). Verify with scoped `cargo build` + `clippy` +
`nix fmt -- <files>` before each.

1. **`switchboard: add GET /hosts + HostInfo`** — the `HostInfo`/`HostTarget`
   types, the SQL, the handler, the route registration; regenerate the OpenAPI
   snapshot and the `.sqlx` cache. Verify: `nextest-db`,
   `switchboard-migrations-consistency` (no schema change expected here, but run
   it), and the OpenAPI guard test.
2. **`treadmill-rs: client job + host methods`** — `post_json`/`delete` helpers
   and the `list_jobs` / `get_job` / `job_events` / `enqueue_job` /
   `terminate_job` / `list_hosts` / `list_image_groups` methods.
3. **`console: job listing + overview pages`** — `GET /jobs`, `GET /jobs/{id}`,
   `POST /jobs/{id}/terminate`, the reused audit feed, the console placeholder,
   nav link, and the relevant CSS.
4. **`console: job dispatch form`** — `GET/POST /jobs/new`, the submit-to-add-rows
   handling, the owner / image-group / parameters / tags / targets / timeout
   fields, the inert host dropdown (with TODO), and the form CSS.

(Phases 2–4 are console/shared-crate only and won't touch the database, so their
gate is `cargo build` + `clippy` + `nextest`; phase 1 is the database-touching
one.)

## 9. Open questions / interim behavior

- With image-group-only init_spec and an **inert host dropdown**, a freshly
  dispatched job matches purely on the host-tag/target textareas the user types
  — confirmed as the intended interim behavior.
- The dispatch form's error UX on a rejected `enqueue` (re-render with the error
  vs. a generic error page) can start simple (generic `PageError`) and be
  refined.
