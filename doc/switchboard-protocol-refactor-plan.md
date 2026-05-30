# Switchboard ↔ Supervisor Protocol Refactor — Implementation Plan

**Status:** proposed roadmap · **Date:** 2026-05-30

> This document is a **one-time implementation roadmap**, not the protocol
> specification. The specification itself is deliberately kept *in the code* to
> avoid drift:
>
> - **Semantics** (message direction, idempotence, reconciliation, lifecycle,
>   evolution rules) live as **Rustdoc** on the protocol types in
>   `treadmill-rs/src/api/switchboard_supervisor.rs` and on the switchboard
>   worker's reconcile function.
> - **The wire contract** is pinned by **committed JSON Schema snapshots**
>   generated from the Rust types via `schemars`; a test fails CI on drift.
>
> Once the phases below are complete, this file may be deleted or archived.

## Goals

1. Make the protocol explicit and reasoned-about rather than implicit in match arms.
2. Keep the spec next to the code (Rustdoc + schema snapshots), never a drifting prose doc.
3. Support non-breaking protocol evolution with an enforced policy.
4. Design all critical state transitions to be idempotent and order-independent.

## Source of truth

| Concern | Source of truth | Drift guard |
|---|---|---|
| Wire format (shapes/tags/fields) | Rust protocol types | committed `*.schema.json` snapshot + diff test |
| Message direction & semantics | Rustdoc on the directional enums | code review |
| Connection lifecycle / reconciliation | Rustdoc on worker reconcile fn | code review |
| Evolution policy | module-level `//!` Rustdoc | schema diff classifiable as additive vs breaking |

---

## Phase 0 — Discovery (done)

Confirmed the switchboard DB model backing reconciliation:

- `supervisors.current_job: Option<Uuid>` (FK → `jobs.job_id`) is the
  **authoritative "assigned job" pointer** (`J_sb`). The worker already
  row-locks the `supervisors` row via `lock_and_get_current_worker` /
  `with_txn`.
- `jobs.functional_state ∈ {queued, dispatched, finalized}` is the coarse
  persisted state, with CHECK constraints:
  - `dispatched` ⇒ `started_at` and `dispatched_on_supervisor_id` non-null,
  - `finalized` ⇔ `exit_status` non-null.
- Fine-grained `RunningJobState` (`Initializing{stage}/Ready/Terminating/
  Terminated`) is **not** persisted as a column today (only appended to the
  `job_events` jsonb log). The abandoned `SqlExecutionStatus` enum at
  `switchboard/src/sql/job.rs:327` is evidence of an earlier attempt.
- **Conclusion:** `functional_state` is a flawed abstraction (it conflates
  scheduling phase with execution phase and drops the real running state); it is
  replaced in Phase 6.1.

---

## Phase 1 — Directional message split

In `treadmill-rs/src/api/switchboard_supervisor.rs`, replace the single
`Message` enum with two direction-typed enums:

```text
SwitchboardToSupervisor :: StartJob | StopJob | StatusRequest(Request<()>) | ProtocolError
SupervisorToSwitchboard :: StatusResponse(Response<ReportedSupervisorStatus>) | SupervisorEvent | ProtocolError
```

- Keep `#[serde(tag = "type", content = "message")]`.
- Deletes both `unimplemented!()` arms in `connector/ws/src/lib.rs:330-337`
  (illegal directions become unrepresentable).
- Types the switchboard worker's `Text` arm in
  `switchboard/src/supervisor_ws_worker.rs:292` and the connector's dispatch.
- Drop or relocate the now-redundant `Message::request_id` /
  `to_response_message` helpers onto the directional enums.

---

## Phase 2 — Handshake & version negotiation

Two-level versioning so additive change is non-breaking:

- **Major** rides the WebSocket subprotocol token (`treadmillv1`,
  `treadmillv2`, …). Standard subprotocol negotiation selects a common token;
  an incompatible peer fails the HTTP upgrade cleanly. **Major bump = breaking.**
- **Minor + feature flags** ride the handshake config:
  - Supervisor advertises its minor in a request header (e.g.
    `tml-protocol-minor`).
  - Switchboard replies with the existing `tml-socket-config` response header,
    now a real struct (`ServerHello { protocol: { major, minor }, features }`).
  - Effective minor = `min(client, server)`. A peer may only **emit** a
    feature/variant once the negotiated minor ≥ its introduction.

**Keepalive stays out of negotiation.** Each side runs its own keepalive with
local config (per the symmetric-keepalive design). Follow-up cleanup: wire the
connector's hardcoded 10s/60s (`connector/ws/src/lib.rs:364-367`) to its own
local config — *not* into `SocketConfig`/`ServerHello`.

### Evolution policy (documented as module Rustdoc)

1. Protocol types never use `#[serde(deny_unknown_fields)]` (older receivers ignore new fields).
2. New fields are additive only: `Option<T>` or `#[serde(default)]`.
3. Adding a message variant requires a **minor** bump and must not be emitted below the negotiated minor (older peers can't deserialize an unknown tag).
4. Removing/renaming fields, changing types, or changing tags = **major** bump (new subprotocol token).

The committed schema diff (Phase 4) makes each PR classifiable as additive (minor) vs breaking (major).

---

## Phase 3 — Protocol-level errors

Enumerated WebSocket close codes in the RFC 6455 private range (4000–4999):

| Code | Name | Meaning |
|---|---|---|
| 4000 | ProtocolViolation | malformed / unexpected message |
| 4001 | UnsupportedVersion | no common major/minor |
| 4002 | InternalError | unexpected failure on the sending side |
| 4003 | Superseded | (optional) switchboard replaced this connection |

Plus an in-band `ProtocolError { code, detail: String }` variant in **both**
directions for diagnostics. **Contract:** on a fatal violation, the offended
side SHOULD send `ProtocolError`, then `Close` with the mapped code, then
terminate. Because critical transitions are idempotent and reconnect re-syncs
state, there is no in-band *recovery* protocol — errors are log → close →
reconnect. The in-band message exists purely for better logs/diagnostics.

---

## Phase 4 — Wire schema drift guard

- Add `#[derive(schemars::JsonSchema)]` to all protocol types.
- Commit snapshots under `treadmill-rs/protocol-schema/`:
  `switchboard_to_supervisor.schema.json`, `supervisor_to_switchboard.schema.json`,
  `server_hello.schema.json`.
- A test regenerates the schema in memory and `assert_eq!`s against the
  committed files; on drift it fails with: run `UPDATE_SCHEMA=1 cargo test -p treadmill-rs`.
- Baseline is snapshot + human review. Optional later: auto-classify the diff
  as breaking/non-breaking.

---

## Phase 5 — Reconciliation contract

**Principle:** the **supervisor** is ground truth for what is *physically
executing*; the **switchboard** is the source of truth for what is *assigned*
(`supervisors.current_job`). On (re)connect the worker sends `StatusRequest`,
awaits the correlated `StatusResponse` (timeout ⇒ treat as dead peer), then
applies idempotent commands/DB transitions to converge. All writes go through
`with_txn` (takeover/staleness guard). Let `J_sb = supervisors.current_job` and
`J_sup` = the supervisor's reported `OngoingJob`.

| # | Switchboard (`J_sb`) | Supervisor reports | Resolution |
|---|---|---|---|
| 1 | none | Idle | aligned; no action |
| 2 | none | `OngoingJob(J)` | unassigned/zombie → `StopJob(J)`; do not adopt |
| 3 | `J_sb` | Idle | job lost → finalize `J_sb` as `SupervisorDroppedJob`; honor `RestartPolicy` (may re-issue `StartJob`) |
| 4 | `J_sb` | `OngoingJob(J_sb)` (same id) | adopt reported `job_state`: DB := reported state |
| 5 | `J_sb` | `OngoingJob(J_sup)`, `J_sup ≠ J_sb` | finalize `J_sb` as `SupervisorDroppedJob` (+ RestartPolicy); `J_sup` is unassigned → `StopJob(J_sup)` |

Every resolution is an idempotent command or a `job_state`-guarded transition
(see Phase 6), so replaying reconciliation is safe. This table lives as Rustdoc
on the worker's `reconcile` function.

### Command idempotence (already-decided semantics)

- `StartJob` / `StopJob` carry the `job_id` and are idempotent: the supervisor
  rejects them if they don't apply to the current job state.
- `StopJob` applies regardless of state as long as the job exists.
- Console-log delivery is best-effort; loss is acceptable.

---

## Phase 6 — Job-state management changes (DB layer)

### 6.1 Replace `functional_state` with a faithful `job_state`

`functional_state` (`queued | dispatched | finalized`) is flawed: it conflates
the **switchboard scheduling phase** with the **supervisor execution phase**,
and its single `dispatched` bucket discards the supervisor's actual reported
`RunningJobState` (which then survives only in the `job_events` log). Rather
than bolt a second `execution_state` column alongside it — which would
reintroduce exactly the kind of two-fields-can-disagree hazard we are removing
elsewhere (see §6.2) — **replace `functional_state` with a single `job_state`
enum that tracks the real lifecycle**, mirroring the wire-level
`RunningJobState`:

```text
job_state ∈ {
  queued,         -- accepted, no supervisor assigned yet (switchboard-owned)
  initializing,   -- + initializing_stage; assigned & starting up
  ready,          -- assigned & running
  terminating,    -- assigned & shutting down
  finalized,      -- terminal; + exit_status, terminated_at
}
```

- **Ownership:** the switchboard owns `queued` and the `queued → initializing`
  dispatch transition (set optimistically to `initializing(starting)` when
  `StartJob` is sent); the supervisor owns the `initializing → ready →
  terminating` sub-states, mirrored into the column from `SupervisorEvent`s and
  on reconciliation (case 4 lands the reported `RunningJobState` directly here);
  the switchboard owns the transition into `finalized` (writing `exit_status`).
- **CHECK constraints** (replacing the existing `valid_queued_implication` /
  `valid_dispatched_implication` / `exit_status_nullity_iso_finalized`):
  - `dispatched_on_supervisor_id` and `started_at` non-null ⇔ `job_state ∈
    {initializing, ready, terminating}`;
  - `exit_status` (and `terminated_at`) non-null ⇔ `job_state = finalized`;
  - `initializing_stage` non-null ⇔ `job_state = initializing`.
- **`job_events` stays** as the append-only audit log; `job_state` is the
  materialized current state. This supersedes the abandoned `SqlExecutionStatus`
  enum at `sql/job.rs:327` (delete it).
- This is a breaking DB migration (new enum type, column swap, dropped
  constraints). Acceptable: the component is mid-refactor, and on reconnect the
  authoritative running state is re-derived from the supervisor anyway, so no
  in-flight data needs preserving.

### 6.2 Canonicalize the supervisor↔job link

`supervisors.current_job` is the **canonical** "assigned job" pointer. Add
enforcement so it cannot diverge from `jobs.dispatched_on_supervisor_id`:

- Document the invariant `supervisors.current_job = J ⇒
  jobs[J].dispatched_on_supervisor_id = that supervisor`.
- Add a partial unique index so a job is `current_job` for at most one
  supervisor.
- Add a CHECK/trigger (or assert it in the worker's `with_txn` transitions) for
  the cross-table invariant.

### 6.3 Transactional "drop + maybe restart" helper

For reconciliation cases 3/5: finalize the lost job with `exit_status =
SupervisorDroppedJob`, clear `supervisors.current_job`, and — if
`remaining_restart_count > 0` — insert a `JobInitSpec::RestartJob` successor
(path already exists in `sql/job.rs:insert`). One transaction.

### 6.4 All job-state mutations go through `with_txn`

Route every reconciliation/job-state write through `with_txn` so the
takeover/staleness guard covers them; keep the "no non-DB awaits inside the
closure" rule (`supervisor_ws_worker.rs:131`). Make transitions idempotent by
guarding on the current `job_state`.

---

## Phase 7 — Resume worker TDD

With directional types, handshake, error model, and the reconciliation contract
pinned, the worker's `Text` arm becomes parse → match the directional enum →
dispatch, and the `reconcile` function is test-driven directly against the
Phase 5 table.

---

## Suggested ordering

Phase 1 → 2 → 3 → 4 (wire/contract foundation) can land first and are mostly
mechanical. Phase 5 + 6 (reconciliation + DB) are the design-heavy core and
should land together. Phase 7 resumes the existing TDD effort on top.
