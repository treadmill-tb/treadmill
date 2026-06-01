# Audit Log & Traceability — Design Plan

Status: design agreed, not yet implemented.

## 1. Goals & non-goals

Build a **curated, durable audit log** of system state changes, persisted in
Postgres, queryable by related entity, and rendered for clients at view time.

Decisions locked in design review:

- Three concerns kept strictly separate (§2).
- An event's **relations** and **visibility** are intrinsic to its *type*; the
  **actor** is instance/runtime data carried on the event (§4).
- Visibility is **per-related-entity, exact-permission match** — no permission
  implication (§5).
- Data is **rendered at view time**; the DB stores raw structured payloads (§4).
- A **witness token** makes a state mutation un-compilable without producing its
  audit event, in the same transaction (§6). No CI lint — the witness is the
  guarantee; raw `sqlx` DML bypass is an accepted, reviewable residual.
- **Every persisted state transition** emits an event, including job
  `initializing` sub-stages (§9).
- Events are **durable across deletion** of the entities they reference (§3).
- **Big-bang migration**: every existing mutation path is routed through the
  audited chokepoint as part of this work (§9).

Non-goals (deferred, not designed here): retention / GC / archival; the
cross-entity "everything I can see" global feed (only the entity-scoped feed is
in scope); permission-implication in the auth engine; localization.

## 2. Separation of concerns

Module header doc-comment (drop on the new `audit` module, replacing the empty
`src/log/mod.rs`):

```rust
//! # Audit & observability
//!
//! Switchboard separates three concerns that are often conflated. They share a
//! vocabulary (the typed event structs in this module) but NOT an emission path:
//!
//! 1. Audit log (this module -> Postgres). Durable, gapless, immutable,
//!    transactional, queried by entity. The record of *what the system did*.
//!    Written in the same transaction as the state change it describes.
//!
//! 2. Operational telemetry (`tracing` -> console/Sentry). Best-effort, sampled,
//!    ephemeral; for debugging and alerting. Carries correlation ids that are
//!    stamped onto audit rows, but is NEVER the source of truth for the audit log.
//!
//! 3. Client-facing event feed (read API). A product surface that renders audit
//!    rows for a viewer, filtered by that viewer's permissions on the related
//!    entities.
//!
//! Audit rows are persisted, then published to (2)/(3) only AFTER the
//! transaction commits -- a rolled-back state change never announces an event.
```

## 3. Data model

New schema objects (added to `SCHEMA.sql` as canonical + a migration in
`migrations/`). All ids are UUIDv7 (time-ordered, index-friendly) where minted
by us.

```sql
-- Append-only audit record. One row per state change (or observational event).
create table tml_switchboard.audit_events (
    event_id       uuid        not null primary key,         -- UUIDv7
    event_type     text        not null,                     -- e.g. 'job_finalized.v1'
    payload        jsonb       not null,                      -- raw structured data; rendered at view time
    actor_id       uuid        not null,                      -- the subject that caused it; plain uuid (durable)
    correlation_id uuid,                                      -- tracing trace-id, if any
    created_at     timestamp with time zone not null default current_timestamp
);
create index audit_events_created_at_idx on tml_switchboard.audit_events (created_at);

create type tml_switchboard.audit_entity_kind as enum ('job', 'host', 'subject');
create type tml_switchboard.audit_role        as enum ('actor', 'subject', 'context');

-- Links an event to an entity it relates to, with the view policy for THIS
-- relation. entity_id is a PLAIN uuid with NO foreign key: that is what keeps
-- audit history alive after the referenced job/host/subject is deleted.
-- view_policy is the exact permission a viewer must hold on `entity` to see the
-- event through this relation, or 'operator_only' for system-internal events
-- that never surface in a user feed.
create table tml_switchboard.audit_event_relations (
    event_id    uuid                              not null
                  references tml_switchboard.audit_events (event_id) on delete cascade,
    entity_kind tml_switchboard.audit_entity_kind not null,
    entity_id   uuid                              not null,
    role        tml_switchboard.audit_role        not null,
    view_policy text                              not null,   -- 'read'|'start'|'ssh'|'stop'|'manage'|'operator_only'

    primary key (event_id, entity_kind, entity_id, role)
);
-- The feed's hot path: "events for entity (kind,id), newest first".
create index audit_event_relations_entity_idx
    on tml_switchboard.audit_event_relations (entity_kind, entity_id, event_id desc);
```

Immutability: a `before update or delete` trigger raises (same pattern as
`deny_irrevocable_grant_change`, SCHEMA.sql:605). Events are never updated or
deleted in normal operation; the relation cascade exists only for a future GC
that deletes whole events deliberately.

No FK on `entity_id`/`actor_id` is a deliberate trade: we give up referential
enforcement to gain provenance that outlives the resource (matches the schema's
`on delete set null` ownership rationale, SCHEMA.sql:31).

### System actor

Worker-driven transitions (job lifecycle, host assignment) have no human actor.
Seed a well-known `system` subject with a hard-coded UUID — same approach as the
`admins` group (SCHEMA.sql:86) — in `SCHEMA.sql` and reference it as the actor
for worker-originated events.

```sql
insert into tml_switchboard.subjects (subject_id, kind)
values ('00000000-0000-0000-0000-000000000002', 'system');
```

## 4. Rust event model

```rust
/// Static identity + visibility of an audit event type. Most impls are
/// generated by `define_event!`.
pub trait AuditEvent: Serialize {
    /// Stable, versioned discriminant, e.g. "job_finalized.v1".
    fn event_type(&self) -> &'static str;
    /// The subject that caused this event (carried on the instance).
    fn actor(&self) -> Uuid;
    /// Entities this event touches, with role + view policy, filled from `self`.
    fn relations(&self) -> Vec<Relation>;
}

pub struct Relation {
    pub entity: EntityRef,     // Job(Uuid) | Host(Uuid) | Subject(Uuid)
    pub role:   Role,          // Actor | Subject | Context
    pub view:   ViewPolicy,
}

pub enum ViewPolicy {
    /// Holders of this EXACT permission on `entity` may see the event.
    Permission(Permission),    // Host(HostPermission) | Job(JobPermission)
    /// System-internal: only global-authority (admins) ever see it.
    OperatorOnly,
}
```

- **Render at view time.** Nothing is rendered on write. A registry maps
  `event_type` -> a `fn(payload: &serde_json::Value, viewer: &ViewerCtx) ->
  RenderedEvent`. Use a link-time registry (`inventory` or `linkme`) so each
  `define_event!` self-registers and the read API never needs a central match.
  The renderer may redact fields based on the viewer.
- **Secrets rule (hard):** audit payloads MUST NOT embed secret values (e.g.
  `parameter_value.is_secret` content). Store the key name + a redacted marker.
  Render-time redaction is defense-in-depth, not the only barrier, because raw
  `payload` jsonb lives in the DB.
- Old event versions accumulate in the binary forever (their `define_event!`
  stays) — accepted; needed to deserialize/render historical rows.

## 5. Visibility & the read API

Exact-permission match, disjunctive across relations: a viewer sees an event if
they satisfy **any** one relation's `view_policy`.

Entity-scoped feed — "events for job X":

1. Caller's access to X is already proven by the route's auth extractor.
2. Compute the **set** of permissions the viewer holds on X in one query
   (extend `auth/engine.rs` with `host_permissions(...) -> Vec<HostPermission>`
   and `job_permissions(...)`: admins/owner => all variants; otherwise the
   granted set).
3. Return events linked to X whose `view_policy` is in that set, newest first,
   using `audit_event_relations_entity_idx`. `operator_only` rows are excluded
   for non-admins; admins (global authority) see everything.

This is a pure SQL predicate — no per-row async auth calls. Because the engine
treats an owner as implicitly holding every permission (engine.rs ownership
branch), owners see all non-operator events on their resources even under exact
match; grantees see only events matching a permission they actually hold.

## 6. Emission & atomicity — the witness chokepoint

Two entry points in the `audit` module:

- `transition()` — the enforced path for **state changes**: bundles the write
  and its event in one transaction.
- `emit()` — for **observational** events that perform no mutation (e.g. an
  access that should be recorded). Still takes `&mut PgConnection` so it joins
  the surrounding transaction.

```rust
/// Proof of being inside an audited transition. Its only field is private to
/// the `audit` module, so it can be constructed ONLY by `audit::transition`.
pub struct WriteToken(());

pub trait Transition {
    type Output;
    type Event: AuditEvent;
    /// Performs the mutation AND returns the event describing it. Because the
    /// event is built from the same inputs as the write, the two cannot diverge.
    async fn apply(self, txn: &mut PgConnection, w: &WriteToken)
        -> Result<(Self::Output, Self::Event), Error>;
}

pub async fn transition<T: Transition>(txn: &mut Transaction<'_>, t: T)
    -> Result<T::Output, Error>
{
    let w = WriteToken(());                        // only mint site
    let (out, event) = t.apply(txn, &w).await?;    // the only place writes happen
    persist_event(txn, &event).await?;             // same txn => atomic
    Ok(out)
}

// Every mutating SQL helper gains a `&WriteToken` parameter:
async fn set_job_state(txn: &mut PgConnection, job: Uuid, s: JobState, _w: &WriteToken) { /* ... */ }
```

Guarantee: a write helper requires `&WriteToken`; `WriteToken` is mintable only
inside `transition`; `transition` always persists the event in the same txn. So
any helper-mediated mutation is audited, atomically, and the event provably
describes that change. The private field means even code inside the `sql`
module cannot forge a token. Accepted residual: a raw `sqlx::query!("UPDATE
...")` that takes no token at all is not caught by types — handled by review,
not a lint (per design decision).

## 7. Post-commit publish

`transition`/`emit` only persist. After the caller commits, the persisted
events are published to operational sinks (console + Sentry) and any live feed
subscribers. Mechanism: return the events from the transaction boundary and log
them in the handler after commit; a `LISTEN/NOTIFY` outbox can be added later
for async consumers. A rolled-back transaction therefore never announces an
event.

## 8. tracing / Sentry integration

- `tracing` remains the operational-logging transport; Sentry attaches via a
  `tracing` error layer (errors/exceptions only, not business events).
- Each request/worker span carries a `correlation_id` (trace id). `transition`
  reads it from the current span and stamps it onto `audit_events.correlation_id`,
  tying an audit row back to its operational logs.

## 9. The `define_event!` macro

Generates the struct, `Serialize`/`Deserialize`, the `AuditEvent` impl
(`event_type` with embedded version, `actor`, `relations`), the view-time
renderer, and the registry entry.

```rust
define_event! {
    /// Job reached a terminal state.
    JobFinalized v1 {
        actor:  Subject,
        job:    Job          @ view(Read),
        host:   Option<Host>  @ view(Read),
        reason: TerminationReason,
        exit:   TaskExitStatus,
    }
    render = "job {job} finalized: {reason} (workload: {exit})";
}
```

Fields typed `Job`/`Host`/`Subject` (or `Option<_>`) become relations with the
declared role/view; plain fields are payload-only.

## 10. Big-bang migration: write sites -> transitions

Every existing mutation is moved behind `transition()`/`emit()`. Inventory
(from `grep`), each needs an event type + `Transition` impl:

| Site | Mutation | Event(s) |
| --- | --- | --- |
| `sql/host.rs:31` | insert `hosts` | `host_registered` |
| `sql/job.rs:191` | insert `jobs` | `job_enqueued` |
| `sql/job.rs:469,513,577,608` | update `jobs` (state) | `job_state_changed` incl. `initializing` sub-stages |
| `sql/job.rs:538,634` | update `hosts.current_job` | `host_job_assigned` / `host_job_cleared` |
| `sql/job/parameters.rs:76` | insert `job_parameters` | folded into `job_enqueued` (secret values redacted) |
| `sql/api_token.rs:107` (+ cancel) | insert/cancel `api_tokens` | `token_issued` / `token_canceled` |
| `sql/user.rs:124,132,143,99,49` | insert subject/user/identity/email | `user_provisioned` / `identity_linked` / `email_added` |
| `sql/user.rs:78,87` | update user / identity login | `user_updated` |
| `sql/user.rs:195,204` | delete/insert `group_members` | `membership_changed` (carries `source`) |
| `supervisor_ws_worker.rs:838,913,984` | update `hosts` (assign / state / rename) | host transitions, actor = **system** subject |

Exempt (transient, not audited): `sql/oauth_flow.rs:15,34` — CSRF flow
bookkeeping, swept on expiry. `supervisor_ws_worker.rs:764-805` inserts need a
check for `#[cfg(test)]` before deciding.

Granularity: each job `job_state` change is its own event, and the
`initializing_stage` sub-steps (starting/fetching_image/allocating/
provisioning/booting, SCHEMA.sql:349) each emit too.

## 11. Implementation phases

1. **Schema**: add `audit_events`, `audit_event_relations`, enums, immutability
   trigger, system subject seed — to `SCHEMA.sql` + a migration. Regenerate
   `.sqlx`.
2. **Core types**: `audit` module (replacing `src/log/mod.rs`), `AuditEvent`
   trait, `Relation`/`ViewPolicy`/`EntityRef`, `persist_event`, view-time
   registry.
3. **Chokepoint**: `WriteToken`, `Transition`, `transition()`, `emit()`.
4. **Macro**: `define_event!` + first event types.
5. **Engine extension**: `host_permissions`/`job_permissions` (held-permission
   set) in `auth/engine.rs`.
6. **Read API + render**: entity-scoped feed endpoint, viewer-aware rendering.
7. **Migration**: route every write site in §10 through a `Transition`; wire
   `system` actor for worker paths; correlation-id stamping.
8. **Post-commit publish**: console/Sentry fan-out; Sentry `tracing` error layer.

## 12. Open items (deferred)

- Retention / GC / archival export.
- Cross-entity "everything I can see" global feed.
- Permission implication in the engine (today: exact match).
- `LISTEN/NOTIFY` outbox for async live subscribers.
