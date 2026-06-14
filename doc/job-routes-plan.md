# Job Enqueue/Get API Routes — Implementation Plan

**Status:** done — both commits landed on `dev/switchboard-refactor`
· **Date:** 2026-06-14

> One-time implementation roadmap for the user-facing `POST /jobs` (enqueue) and
> `GET /jobs/{id}` (fetch) switchboard routes. Self-contained so a contributor
> (human or agent) can pick up the work without the design conversation that
> produced it. As with the other plans under `doc/`, the durable specification
> lives in the code once written (Rustdoc + the committed OpenAPI snapshot); this
> file may be deleted or archived once the routes land.

---

## 1. Scope

Implement the two user-facing job routes that the switchboard refactor left
stubbed in `switchboard/src/routes/mod.rs`:

- **`POST /jobs`** — enqueue a new job (insert a `queued` row; the polling
  scheduler picks it up later — enqueue is fire-and-forget).
- **`GET /jobs/{id}`** — fetch one job's full lifecycle/status info.

Out of scope (separate endpoints, not in this plan): `GET /jobs` (list),
`DELETE /jobs/{id}` (stop). Also deferred (recommended follow-ups): typed client
methods (`SwitchboardClient::enqueue_job` / `get_job`) and a `JobEnqueued` audit
event (until the latter lands, the existing `/jobs/{id}/events` feed stays empty
for enqueue).

## 2. Grounding (confirmed by inspection)

- **Enqueue is fire-and-forget.** `Scheduler::tick` (`switchboard/src/scheduler.rs`)
  polls `job_state='queued'` on `match_interval`; there is no notify channel and
  no synchronous "did it match a host" signal. The old
  `SubmitJobError::SupervisorMatchError` is gone; match/no-match surfaces only
  through job state + the events feed.
- **`sql::job::insert()`** (`switchboard/src/sql/job.rs`) exists but has no
  production caller and does **not** set `owner_id`. The restart-successor path
  (`finalize_dropped_and_maybe_restart`) also leaves it NULL — a latent bug, since
  `can_access_job` authorizes via `jobs.owner_id` (a NULL-owner job is unreadable
  even by its creator). This plan fixes it: enqueue sets the owner; restart
  inherits the predecessor's.
- **Read authz primitive** is ready: `engine::can_access_job(pool, user, job,
  perm)` (`switchboard/src/auth/engine.rs`) returns `false` (→403) for a
  nonexistent job, so existence does not leak. Same gate the existing
  `POST /jobs/{id}/log-token` route uses.
- **No `JobInfo`/`JobStatus` DTO** exists in the current API module — the old one
  was stripped. GET needs a freshly designed response type.
- **`principals($user)`** SQL function returns the user plus the groups it
  (transitively) belongs to; `requested_owner ∈ principals(user)` is exactly "the
  caller, or a group the caller is a member of" (the `images.rs::subject_reaches`
  pattern).

## 3. Locked design decisions

| # | Decision | Rationale |
|---|---|---|
| Routes | `POST /jobs`, `GET /jobs/{id}` | REST-conventional; collapses the stubbed `/jobs/new`, `/jobs/{id}/status`, `/jobs/{id}/info` |
| Owner | Request may name an owner; must satisfy `requested ∈ principals(caller)` (the caller or a group it belongs to), else 403. Absent ⇒ owner = caller | Lets a user file a job under a group they belong to; can't donate jobs to arbitrary subjects |
| Resume/restart authz | Caller needs **Manage** on the referenced job, else 403 | Spawning a successor / resuming exposes the referenced job; Manage is the control-level permission |
| Resume/restart + custom owner | Both checks independent, both must pass; the successor's owner is the *requested* one (not inherited from the referenced job) when `owner` is set | The two concerns are orthogonal |
| Image validation | **Deferred** to the scheduler | An unresolvable image finalizes the job as `image_error` at dispatch, visible via GET/events; avoids duplicating dispatch logic + races with deregistration |
| Eligibility / fail-fast | **Not** checked at enqueue; leave a `TODO(authz)` echoing the scheduler's. The scheduler is the final, authoritative gate | A never-matchable job sits `queued` until `default_queue_timeout` culls it; fail-fast may come later |
| GET parameters | Included, **secret values redacted** (`value: None` when secret) | Status feed is useful; secrets must not leak |
| GET image shape | The four mutually-exclusive nullable columns collapsed into one `JobImageRef` enum | Cleaner for clients than four `Option`s |
| 403 vs 404 | `can_access_job` false ⇒ 403 for both unauthorized and nonexistent | Matches the log-token route; no existence leak |

## 4. Steps

Two reviewable, end-to-end commits. Read path first (lower risk, and it forces
the `JobInfo` DTO that POST's tests also lean on).

### Commit A — `GET /jobs/{id}`

- **A1. Shared DTOs** (`treadmill-rs/src/api/switchboard/jobs.rs`): add
  `JobParameterView { secret: bool, value: Option<String> }` (`value: None` ⇔
  secret); `JobImageRef` enum (`Image{digest}` / `ImageGroup{digest}` /
  `Resume{job_id}` / `Restart{job_id}`); `JobInfo { job_id, owner_id, state,
  initializing_stage, image, resolved_image_digest, ssh_keys, restart_policy,
  host_tag_requirements, target_requirements, parameters: HashMap<String,
  JobParameterView>, timeout_secs, queued_at, started_at, dispatched_on_host_id,
  ssh_endpoints, termination_reason, task_exit_status, exit_message,
  terminated_at, last_updated_at }`. Derive `JsonSchema, Serialize, Deserialize,
  Debug, Clone`.
- **A2. Public `JobState`** (`treadmill-rs/src/api/switchboard.rs`): enum
  Queued/Scheduled/Initializing/Ready/Terminating/Finalized (`JsonSchema`, serde
  `snake_case`). Reuse existing shared `JobInitializingStage`, `TaskExitStatus`,
  `RestartPolicy`, `TerminationReason`.
- **A3. SQL → DTO converter** (`switchboard/src/sql/job.rs`): `From<SqlJobState>
  for JobState`; `SqlSshEndpoint → api` mapping; `impl SqlJob { async fn
  into_info(self, &mut PgConnection) -> Result<JobInfo, sqlx::Error> }` (fetches
  target requirements + parameters, redacts secret params, folds the four image
  columns into `JobImageRef`, `job_timeout → timeout_secs`).
- **A4. Handler** (`switchboard/src/routes/jobs.rs`): `get_job(State, Subject,
  Path<Uuid>) -> Result<Json<JobInfo>, StatusCode>` — `can_access_job(…, Read)`
  → 403 on false → `fetch_by_job_id` → `into_info` → `Json`; 500 + `error!` on DB
  error.
- **A5. Wire** (`switchboard/src/routes/mod.rs`): replace the `/jobs/{id}/status`
  + `/jobs/{id}/info` stubs with `.api_route("/jobs/{id}", get_with(jobs::get_job,
  |o| o))`.
- **A6. Tests** (`switchboard/tests/job_routes.rs`): owner reads; admin reads
  any; stranger → 403; nonexistent → 403; secret param redacted, non-secret in
  clear. `#[ignore]` per AGENTS.md §6.
- **A7. Snapshots + verify**: `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard
  --test openapi_spec`; regen `.sqlx` if a `query!` changed (§4) + offline
  `--workspace --all-targets` build; scoped build + clippy; `nix fmt -- <files>`;
  `nextest-db`. Commit.

### Commit B — `POST /jobs`

- **B1. Request type** (`treadmill-rs/src/api/switchboard.rs`): add `#[serde(default)]
  pub owner: Option<Uuid>` to `JobRequest`. Add `EnqueueJobResponse { job_id }` in
  `jobs.rs`.
- **B2. Thread owner through insert** (`switchboard/src/sql/job.rs`): `insert(...)`
  gains `owner: Option<Uuid>`, adds `owner_id` to the INSERT column list; update
  `finalize_dropped_and_maybe_restart` to pass `predecessor.owner_id`. Regen
  `.sqlx`.
- **B3. Handler** (`switchboard/src/routes/jobs.rs`): `enqueue(State, Subject,
  Json<JobRequest>) -> Result<(StatusCode, Json<EnqueueJobResponse>), StatusCode>`:
  1. effective owner = `req.owner` (validate `∈ principals(user)`, else 403) else
     `subject.user_id()`;
  2. if `init_spec` is `Resume`/`Restart`: `can_access_job(caller, referenced,
     Manage)` → 403 on false;
  3. `timeout = req.override_timeout.unwrap_or(config.service.default_job_timeout)`
     → `PgInterval` (reject ≤0 → 422);
  4. one txn: `job::insert(req, id, subject.token_id(), owner, timeout, now,
     &mut txn)` + `parameters::insert(id, req.parameters, …)`; commit;
  5. `201 { job_id }`. `// TODO(authz):` echoing the scheduler's eligibility TODO.
- **B4. Wire** (`switchboard/src/routes/mod.rs`): replace the `/jobs/new` stub with
  `.api_route("/jobs", post_with(jobs::enqueue, |o| o))`.
- **B5. Tests** (`switchboard/tests/job_routes.rs`): enqueue → row `queued`,
  `owner_id` = caller; `owner` = a group caller belongs to → accepted; `owner` =
  unrelated subject → 403; resume/restart of a job caller can't Manage → 403;
  `override_timeout` honored + default applied. `#[ignore]`.
- **B6. Snapshots + verify**: same as A7 (OpenAPI regen, `.sqlx` regen for the
  `insert` change, offline build, build/clippy/fmt, `nextest-db`). Commit.

## 5. Notes

- No schema migration in either commit (the `owner_id` column already exists), so
  `switchboard-migrations-consistency` is untouched.
- These are client API types, not supervisor wire types, so `protocol_schema.rs`
  is not regenerated; the **OpenAPI** snapshot is, once the routes are wired.
