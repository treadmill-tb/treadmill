-- -*- fill-column: 80; -*-

create schema tml_switchboard;

-- ===========================================================================
-- SUBJECTS: users and groups, unified
-- ===========================================================================
--
-- Users and groups are both "subjects": principals that can (a) own resources,
-- (b) be granted permissions on resources, and (c) be members of groups. To
-- avoid scattering polymorphic (kind, id) pairs across every ownership,
-- membership, and grant table, both users and groups extend a single
-- `subjects` row. Every ownership/grant/membership FK in this schema therefore
-- references `subjects(subject_id)`, and "a user or a group" is expressible as
-- one foreign key.
--
-- `system` subjects are non-human service actors (the switchboard worker,
-- internal automation). They do not log in via OAuth; they authenticate via API
-- tokens. They are seeded out of band (see FIXTURES.sql), not created by login.
create type tml_switchboard.subject_kind as enum ('user', 'group', 'system');
create table tml_switchboard.subjects
(
    subject_id uuid                         not null primary key,
    kind       tml_switchboard.subject_kind not null
);

-- A human (or system) principal. Authentication is fully external (OAuth); there
-- is no password. `username` is the internal Treadmill handle, suggested from
-- the provider login at first sign-in but freely changeable, hence its own
-- uniqueness independent of any provider identity. `locked` deactivates the
-- account without deleting it (preserving job/audit provenance).
create table tml_switchboard.users
(
    subject_id uuid not null primary key references tml_switchboard.subjects (subject_id) on delete cascade,
    username   text not null unique,
    full_name  text,
    avatar_url text,
    locked     bool not null default false
);

-- The OAuth identity link. Keyed on the provider's STABLE numeric user id (as
-- text), never the login handle -- handles are renameable and reusable, so a
-- handle key would eventually mis-link accounts. `provider_login` records the
-- current handle for display only. A user holds at most one identity per
-- provider. This table is the seam that makes additional providers cheap later.
create table tml_switchboard.user_identities
(
    provider         text not null,
    provider_user_id text not null,
    user_id          uuid not null references tml_switchboard.users (subject_id) on delete cascade,
    provider_login   text,
    linked_at        timestamp with time zone not null default current_timestamp,

    primary key (provider, provider_user_id),
    unique (user_id, provider)
);

-- Verified email addresses, used to link a future provider login to an existing
-- user (same verified email => same person). Globally unique so it can serve as
-- that linking key, multi-row per user, and tagged with the provider that
-- surfaced it. ONLY verified addresses live here (`check (verified)`): linking
-- on an unverified address would allow account takeover by claiming someone
-- else's email on the external provider.
create table tml_switchboard.user_emails
(
    email    text not null primary key,
    user_id  uuid not null references tml_switchboard.users (subject_id) on delete cascade,
    provider text not null,
    verified bool not null,
    added_at timestamp with time zone not null default current_timestamp,

    check (verified)
);

-- A group/organization. Owns resources and aggregates members like any subject.
create table tml_switchboard.groups
(
    subject_id uuid not null primary key references tml_switchboard.subjects (subject_id) on delete cascade,
    name       text not null unique
);

-- The system-global admin group. Its UUID is a hard-coded well-known constant so
-- it can be referenced directly in authorization queries. Global admin authority
-- = membership in this group; orphaned resources (owner_id IS NULL) are
-- manageable only by its members.
insert into tml_switchboard.subjects (subject_id, kind)
values ('00000000-0000-0000-0000-000000000001', 'group');
insert into tml_switchboard.groups (subject_id, name)
values ('00000000-0000-0000-0000-000000000001', 'admins');

-- The system actor. A `system` subject (non-human service principal) with a
-- hard-coded well-known UUID, so the switchboard can attribute the audit events
-- it raises on its own initiative -- worker-driven job lifecycle transitions,
-- host assignment, internal automation -- to a stable actor referenceable
-- directly in code. It is a bare `subjects` row with no `users`/`groups`
-- extension: it never logs in and owns nothing; it exists only to be named as
-- the actor of system-originated events.
insert into tml_switchboard.subjects (subject_id, kind)
values ('00000000-0000-0000-0000-000000000002', 'system');

-- ===========================================================================
-- GROUP MEMBERSHIP: a many-to-many DAG with provenance
-- ===========================================================================
--
-- `member_id` references `subjects`, so a member may itself be a group -- this
-- is what gives nested groups. Membership is therefore a directed graph; we
-- enforce that it stays acyclic (see trigger below).
--
-- `source` records WHO added the membership. The same subject may be both a
-- manually-added member and an auto-group member, so `source` is part of the
-- primary key. Auto-sync (e.g. GitHub org reconciliation) only ever
-- inserts/deletes rows of its own source, so it can never clobber manual
-- memberships, and vice versa. `source_ref` carries the external key (e.g. the
-- GitHub org id) for auto sources and is part of the primary key too: a subject
-- may be in one group via several sources of the same kind (e.g. two GitHub
-- orgs that both feed it), and each must be an independent, separately-reconciled
-- row. It is NOT NULL (default '') so it participates in the PK uniformly;
-- manual rows simply carry the empty string.
create type tml_switchboard.membership_source as enum ('manual', 'github_org');
create table tml_switchboard.group_members
(
    group_id   uuid                              not null references tml_switchboard.groups (subject_id)   on delete cascade,
    member_id  uuid                              not null references tml_switchboard.subjects (subject_id) on delete cascade,
    source     tml_switchboard.membership_source not null,
    source_ref text                              not null default '',

    primary key (group_id, member_id, source, source_ref),

    -- Trivial self-cycle is a cheap declarative constraint; longer cycles need
    -- the reachability trigger below.
    constraint no_self_membership check (group_id <> member_id)
);
-- Upward traversal (member -> the groups it belongs to) hits `member_id`; the PK
-- already indexes the `group_id` prefix for downward traversal.
create index group_members_member_id_idx on tml_switchboard.group_members (member_id);

-- Enforce that the membership graph stays acyclic. Adding the edge "group_id
-- contains member_id" closes a cycle iff member_id can already reach group_id by
-- following existing "contains" edges. Updates are rare and traversal is hot, so
-- paying a reachability check on write only is the right trade.
--
-- Correctness under concurrency: two transactions could each add an edge that is
-- individually acyclic but jointly cyclic, neither seeing the other's
-- uncommitted row. We close that hole *inside the trigger* with a
-- transaction-scoped advisory lock held until commit. It does not conflict with
-- the INSERT's ROW EXCLUSIVE table lock (so no deadlock), and it serializes all
-- membership mutations: a waiter resumes only after the holder commits, and
-- under READ COMMITTED its fresh per-statement snapshot then includes that
-- committed edge -- so any attempt that would close a cycle sees it and aborts.
-- (Assumes READ COMMITTED, the default. Under higher isolation, use SERIALIZABLE
-- instead, whose SSI detects the same conflict and aborts one transaction.)
create or replace function tml_switchboard.group_members_no_cycle()
    returns trigger
    language plpgsql as
$$
begin
    perform pg_advisory_xact_lock(hashtext('tml_switchboard.group_members'));

    if exists (
        with recursive reach(id) as (
            select NEW.member_id
            union
            select gm.member_id
            from tml_switchboard.group_members gm
            join reach r on gm.group_id = r.id
        )
        select 1 from reach where id = NEW.group_id
    ) then
        raise exception
            'adding % as a member of % would create a cycle', NEW.member_id, NEW.group_id;
    end if;

    return NEW;
end;
$$;

-- DELETE can never create a cycle, so the check runs on INSERT/UPDATE only.
create constraint trigger group_members_acyclic
    after insert or update on tml_switchboard.group_members
    for each row execute function tml_switchboard.group_members_no_cycle();

-- Declares that a group's membership is auto-synced from an external source
-- (e.g. a GitHub organization). The reconciler iterates these bindings -- lazily
-- on a member's re-auth and periodically -- and for each one inserts/deletes
-- `group_members` rows with `source = 'github_org'` and `source_ref =
-- external_id`, touching only auto rows so manual memberships are preserved.
--
-- `external_id` is the provider's STABLE identifier for the source (e.g. the
-- GitHub org's numeric id), not its renameable login; `external_name` is kept
-- for display only. A group may sync from several sources, and one source may
-- feed several groups, hence the composite key. `last_synced_at` is null until
-- the first successful reconciliation.
create table tml_switchboard.group_auto_sources
(
    group_id       uuid                              not null references tml_switchboard.groups (subject_id) on delete cascade,
    provider       text                              not null,
    external_id    text                              not null,
    external_name  text,
    membership_via tml_switchboard.membership_source not null,
    last_synced_at timestamp with time zone,

    primary key (group_id, provider, external_id)
);

-- ===========================================================================
-- API TOKENS
-- ===========================================================================
--
-- Programmatic auth for clients that cannot perform an interactive OAuth flow
-- (CLI, CI, system actors). A token currently acts with the FULL authority of
-- its owning subject; per-token scoping is intentionally deferred and can be
-- added later as a `token_grants` table without disturbing this one.
--
-- Tokens have both natural expiration and an explicit revocation mechanism,
-- which voids a token before it expires.
create type tml_switchboard.api_token_revocation as
(
    revoked_at         timestamp with time zone,
    revocation_reason text
);
-- `user_agent` and `created_ip`/`created_port` record the provenance of a token
-- at mint time (the client that requested it), surfaced in the session-list API
-- and mirrored into the `session_token_issued` audit event. `created_ip` is the
-- client address as resolved by the server's trusted-proxy policy (text rather
-- than `inet` so a proxy-supplied value that fails to parse is still recorded
-- verbatim for forensics). `comment` is an optional user-supplied label.
create table tml_switchboard.api_tokens
(
    token_id     uuid                     not null primary key,
    token        bytea                    not null unique,
    user_id      uuid                     not null references tml_switchboard.users (subject_id) on delete cascade,
    revoked      tml_switchboard.api_token_revocation,
    created_at   timestamp with time zone not null,
    expires_at   timestamp with time zone not null,
    user_agent   text,
    comment      text,
    created_ip   text,
    created_port integer,

    check (octet_length(token) = 128)
);

-- ===========================================================================
-- OAUTH FLOWS
-- ===========================================================================
--
-- Short-lived server-side record of an in-flight OAuth authorization-code flow.
-- When the switchboard redirects a user to the provider it mints a random CSRF
-- `state` and persists it here; on callback it looks the state up (consuming the
-- row) to confirm the response corresponds to a flow this server actually
-- initiated. `provider` records which provider the flow targets so the callback
-- knows which token endpoint and identity mapping to use. Rows are deleted on
-- successful callback and swept after `expires_at`; a never-completed flow
-- simply expires.
create table tml_switchboard.oauth_flows
(
    state         text                     not null primary key,
    provider      text                     not null,
    pkce_verifier text,
    created_at    timestamp with time zone not null default current_timestamp,
    expires_at    timestamp with time zone not null
);

-- ===========================================================================
-- HOSTS
-- ===========================================================================

-- Host + Port tuple for ssh_endpoints of a host.
create domain tml_switchboard.port as integer
    check (value >= 0 and value < 65535);
create domain tml_switchboard.ssh_host as text
    check (value is not null);
create domain tml_switchboard.ssh_port as tml_switchboard.port
    check (value is not null);
create type tml_switchboard.ssh_endpoint as
(
    ssh_host tml_switchboard.ssh_host,
    ssh_port tml_switchboard.ssh_port
);

-- All hosts must be registered with the switchboard database: any host not
-- registered will be turned away at the gate, so to speak. A host is the
-- managed device that actually runs the user workload; the supervisor is the
-- software process that drives it and is the WebSocket peer of the switchboard.
-- The `hosts` row carries both the host's user-facing properties (name, tags,
-- ssh_endpoints, owner) and the credentials/state of the supervisor authorized
-- to drive it (auth_token, worker_instance_id) — one row per host.
--
-- The auth_token field authenticates the host's supervisor: a 256-byte random
-- string uniquely identifying the supervisor on connect.
create table tml_switchboard.hosts
(
    host_id            uuid   not null primary key,
    name               text   not null,
    auth_token         bytea  not null unique,
    tags               text[] not null,

    -- Owning subject (user or group). NULL means orphaned: the resource has no
    -- owner and is manageable only by members of the admin group. Resources are
    -- orphaned (not cascade-deleted) when their owner is deleted, hence
    -- `on delete set null`.
    owner_id           uuid references tml_switchboard.subjects (subject_id) on delete set null,

    -- SSH endpoints that all jobs executing on this host are reachable under.
    -- Each endpoint is a "hostname:port" tuple. IPv6 addreses are enclosed in
    -- square brackets, such as "[::1]:22".
    ssh_endpoints      tml_switchboard.ssh_endpoint[] not null,

    -- A host can either be idle or have a job assigned. This column keeps
    -- track of this state. If a job is assigned, then the host is not idle,
    -- and the "sub-state" is determined through the job's state data.
    --
    -- This is the *canonical* "assigned job" pointer. The invariant
    --
    --     hosts.current_job = J  =>
    --       jobs[J].dispatched_on_host_id = this host
    --
    -- is asserted by the worker in its `with_txn` transitions (the worker is the
    -- sole writer). The partial unique index below enforces the half that is
    -- expressible as a pure constraint: a job is `current_job` for at most one
    -- host. FK constraint added after `jobs` is defined below.
    current_job        uuid,

    -- Each host is driven by at most one supervisor connection, serviced by a
    -- worker. To prevent multiple concurrent workers claiming ownership of the
    -- same host, each host--worker combination is assigned a unique,
    -- monotonically increasing "worker instance ID", checked by subsequent DB
    -- operations.
    worker_instance_id bigint NOT NULL DEFAULT 0,

    -- Liveness heartbeat: the current worker refreshes this on each periodic
    -- tick, and clears it to NULL when it disconnects cleanly (without having
    -- been superseded). A host is "live" -- eligible for the scheduler to
    -- dispatch onto -- iff `last_seen_at` is non-null and recent (within the
    -- configured staleness window). NULL ⇒ no connected supervisor; a stale
    -- timestamp ⇒ the worker died silently (the heartbeat catches what a missed
    -- clean disconnect does not). This is the DB-only signal by which the
    -- (out-of-process) scheduler learns which hosts have a live supervisor; no
    -- in-process channel couples them.
    last_seen_at       timestamp with time zone,

    check (octet_length(auth_token) = 128),
    check (worker_instance_id >= 0)
);

-- ===========================================================================
-- JOBS
-- ===========================================================================
--
-- The database job table is an archive of inactive jobs, a persistent store for
-- reconstructing switchboard state after a restart, and a record of final exit
-- status.

-- Restart policy dictates the number of times a job can be restarted
-- automatically by the switchboard.
create type tml_switchboard.restart_policy as
(
    remaining_restart_count integer
);

-- The job's execution lifecycle state.
create type tml_switchboard.job_state as enum (
    -- Accepted, no host assigned yet; eligible for scheduling.
    'queued',
    -- Bound to a host but StartJob not yet sent; no longer eligible for
    -- (re)scheduling. dispatched_on_host_id set, started_at null.
    'scheduled',
    -- Assigned & starting up; carries initializing_stage.
    'initializing',
    -- Assigned & running.
    'ready',
    -- Assigned & shutting down.
    'terminating',
    -- Terminal; carries termination_reason and terminated_at.
    'finalized'
    );

-- The sub-stage of an `initializing` job, mirroring the wire-level
-- `JobInitializingStage`.
create type tml_switchboard.job_initializing_stage as enum (
    'starting',
    'fetching_image',
    'allocating',
    'provisioning',
    'booting'
    );

-- Why a job terminated. Set once, at finalization. Orthogonal to the task's exit
-- status (success/failure of the user workload) and to any exit message.
create type tml_switchboard.termination_reason as enum (
    -- workload-driven (the job's own process ended it)
    'workload_exited',
    'workload_self_canceled',
    -- externally canceled
    'user_canceled',
    -- timeouts
    'queue_timeout',
    'execution_timeout',
    -- bad / unfetchable image (user fault)
    'image_error',
    -- infrastructure failure ("crashed / dead")
    'host_match_error',
    'host_start_failure',
    'host_dropped_job',
    'host_unreachable',
    'resume_failed',
    'internal_error'
    );

-- The host-reported outcome of the user's workload (its "task outcome"), as
-- relayed by the host's supervisor. Set out-of-band of termination and
-- revisable while assigned: `pending` until known, then `success`/`failure`.
-- Once set it is never cleared. Orthogonal to termination_reason.
create type tml_switchboard.task_exit_status as enum (
    'pending',
    'success',
    'failure'
    );

create table tml_switchboard.jobs
(
    job_id                      uuid                             not null primary key,

    -- Owning subject (user or group). NULL means orphaned (see hosts). For a
    -- job started by a user on a group-owned host, the user is the owner; the
    -- owning group's control over the job is conferred separately, as an
    -- irrevocable grant inserted at dispatch time (see job_grants).
    owner_id                    uuid references tml_switchboard.subjects (subject_id) on delete set null,

    -- These specify the job image and the nature of the job. A job's image is
    -- referenced content-addressed against the switchboard image catalog: either
    -- a concrete image (`image_digest`, the OCI manifest digest of a registered
    -- `images` row) or an image group (`image_group_digest`, the index digest of
    -- a registered `image_groups` row) resolved to a concrete member at dispatch.
    --  (1) normal job:    exactly one of image_digest / image_group_digest is
    --                     set; both resume_job_id and restart_job_id are null
    --  (2) restarted job: exactly one of image_digest / image_group_digest is
    --                     set; restart_job_id is set
    --  (3) resumed job:   resume_job_id is set; both image digests are null
    resume_job_id               uuid references tml_switchboard.jobs (job_id) on delete no action,
    restart_job_id              uuid references tml_switchboard.jobs (job_id) on delete no action,
    image_digest                text,
    image_group_digest          text,

    -- The concrete image manifest digest actually dispatched, recorded at
    -- dispatch for reproducibility/audit. For a concrete-image job this equals
    -- `image_digest`; for a group job it is the member the matcher selected.
    resolved_image_digest       text,

    -- SSH keys to be injected, if any
    ssh_keys                    text[]                           not null,

    -- Restart policy
    restart_policy              tml_switchboard.restart_policy   not null,

    -- Token the job was enqueued by. Used to determine which hosts a job
    -- can run on.
    enqueued_by_token_id        uuid                             not null references tml_switchboard.api_tokens (token_id) on delete no action,

    -- Host eligibility: the set of tags a host must carry (as a superset) for
    -- this job to be schedulable onto it. Opaque strings, matched by containment
    -- against `hosts.tags`. Target (DUT) requirements live in the separate
    -- `job_target_requirements` table.
    host_tag_requirements       text[]                           not null default '{}',

    -- The amount of time the job can run before it is killed.
    job_timeout                 interval                         not null,

    -- The latest known execution-lifecycle state of the job.
    job_state                   tml_switchboard.job_state        not null,

    -- The sub-stage while `job_state = 'initializing'`; null otherwise.
    initializing_stage          tml_switchboard.job_initializing_stage,

    -- The time at which the job was queued. If after
    -- [config:api.jobs.queue_timeout] the job is still queued, it is canceled.
    queued_at                   timestamp with time zone         not null,

    -- The time at which the job was started.
    started_at                  timestamp with time zone,

    -- ID of the host the job was dispatched to, so the switchboard can
    -- discern job failure on supervisor reconnection after a dual failure.
    dispatched_on_host_id       uuid,

    -- SSH endpoints that this job is reachable under, populated once scheduled on
    -- a host that exposes endpoints.
    ssh_endpoints               tml_switchboard.ssh_endpoint[],

    -- User-requested cancellation signal: the DB side of user-cancel. When set,
    -- the assigned job's worker converges the job to `finalized` with
    -- `termination_reason = user_canceled`. It is re-read on every reconcile pass
    -- (alongside the execution-timeout deadline), so the decision is always made
    -- against fresh state and composes with a later API that sets this column.
    -- NULL means no cancellation requested. Queued (unassigned) jobs are the
    -- scheduler/reaper's responsibility, not a worker's.
    cancel_requested_at         timestamp with time zone,

    -- Filled out when transitioned into `finalized`. `termination_reason` records
    -- *why* the job stopped; `task_exit_status` records the *result of the user
    -- workload* (independent of the reason); `exit_message` is an optional
    -- human-readable note. Captured workload output is stored in object storage,
    -- not here.
    termination_reason          tml_switchboard.termination_reason,
    task_exit_status            tml_switchboard.task_exit_status,
    exit_message                text,
    terminated_at               timestamp with time zone,

    -- Bookkeeping; set via `last_updated_at = DEFAULT` on UPDATE, per
    -- https://www.morling.dev/blog/last-updated-columns-with-postgres/.
    last_updated_at             timestamp with time zone         not null default current_timestamp,

    ---->> INVARIANT CHECKING <<----

    -- Two allowed init states:
    --  (1) resume_job_id = null, restart_job_id = _, and exactly one of
    --      image_digest / image_group_digest is set
    --  (2) resume_job_id != null, restart_job_id = null, both image digests null
    constraint valid_init_spec check (
      (resume_job_id is null
         and (image_digest is not null)::int + (image_group_digest is not null)::int = 1)
        or (resume_job_id is not null and restart_job_id is null
              and image_digest is null and image_group_digest is null)
    ),

    -- Restart count >= 0
    constraint valid_restart_policy check (
      (restart_policy).remaining_restart_count >= 0
    ),

    -- A host is bound from `scheduled` onwards (through `finalized`);
    -- `dispatched_on_host_id` tracks that binding for the assigned,
    -- not-yet-terminal lifecycle states.
    constraint dispatched_host_iso_assigned check (
      (dispatched_on_host_id is not null) =
        (job_state in ('scheduled', 'initializing', 'ready', 'terminating'))
    ),

    -- `started_at` is set exactly while executing, i.e. once dispatched
    -- (`initializing`) and until terminal.
    constraint started_at_iso_executing check (
      (started_at is not null) =
        (job_state in ('initializing', 'ready', 'terminating'))
    ),

    -- The terminal record is set exactly when `finalized`.
    constraint termination_reason_iso_finalized check (
      (termination_reason is not null) = (job_state = 'finalized')
    ),
    constraint terminated_at_iso_finalized check (
      (terminated_at is not null) = (job_state = 'finalized')
    ),

    -- `task_exit_status` is absent while `queued` (no host assigned yet) and
    -- may be present in any later state.
    constraint task_exit_status_absent_while_queued check (
      task_exit_status is null or job_state <> 'queued'
    ),

    -- `initializing_stage` is present exactly while `initializing`.
    constraint initializing_stage_iso_initializing check (
      (initializing_stage is not null) = (job_state = 'initializing')
    )
);

ALTER TABLE tml_switchboard.hosts
    ADD FOREIGN KEY (current_job)
    REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION;

-- A job may be the `current_job` of at most one host. Partial (only over
-- non-null `current_job`) so idle hosts don't collide on NULL.
CREATE UNIQUE INDEX hosts_current_job_unique
    ON tml_switchboard.hosts (current_job)
    WHERE current_job IS NOT NULL;

-- Host tags are opaque strings (`key=value` pairs or bare flags, by convention
-- only -- the matcher never parses them). A job requests a host whose tags are a
-- superset of the job's `host_tag_requirements`; the GIN index backs that
-- containment (`tags @> required`) lookup over the host pool.
CREATE INDEX hosts_tags_gin ON tml_switchboard.hosts USING gin (tags);

-- ===========================================================================
-- HOST TARGETS (DUTs)
-- ===========================================================================
--
-- A host drives one or more attached targets (devices under test). Each target
-- carries its own opaque tag set (e.g. `board=nrf52840dk`, `ble`, `gpio`),
-- independent of the host's tags. A job requests an array of targets, each by a
-- required tag subset; the scheduler assigns each requested target to a distinct
-- DUT whose tags satisfy it (a bipartite match, done in the application). Target
-- tags do NOT participate in image selection -- that is host-tag-only (see
-- `image_group_members`).
create table tml_switchboard.host_targets
(
    target_id  uuid   not null primary key,
    host_id    uuid   not null references tml_switchboard.hosts (host_id) on delete cascade,

    -- Stable per-host label for the DUT (e.g. "dut0").
    name       text   not null,

    -- Opaque tag set, same convention as host tags.
    tags       text[] not null default '{}',

    unique (host_id, name)
);

-- Backs the per-target containment (`tags @> required`) match during scheduling.
CREATE INDEX host_targets_tags_gin ON tml_switchboard.host_targets USING gin (tags);

-- ===========================================================================
-- PERMISSIONS / ACLs
-- ===========================================================================
--
-- Authorization is ownership + explicit grants. Each resource carries an
-- `owner_id` (above); the grant tables below record additional (subject,
-- permission) entries. A subject is authorized for a permission on a resource if
-- it (or any group it transitively belongs to) owns the resource or holds a
-- matching grant -- the *policy* disjunction is evaluated in application SQL, not
-- here. The one shared building block both sides of that disjunction need -- the
-- set of a subject's transitive group memberships -- is the `principals`
-- function below, so the application query stays free of the recursive CTE.
--
-- `principals(arg_subject)` returns the subject itself plus every group it
-- reaches by following `member_id -> group_id` edges. It is a parameterized
-- function rather than a view so it is seeded with a single subject and walks
-- only that subject's memberships via `group_members_member_id_idx`, instead of
-- materializing the closure for every subject. Ownership and grant checks are
-- tested against this whole set (see `src/auth/engine.rs`).
create or replace function tml_switchboard.principals(arg_subject uuid)
    returns table (id uuid)
    language sql
    stable as
$$
    with recursive reached(id) as (
        select arg_subject
        union
        select gm.group_id
        from tml_switchboard.group_members gm
        join reached r on gm.member_id = r.id
    )
    select id from reached;
$$;

--
-- `manage` is the meta-permission: holding it lets a subject edit the resource's
-- ACL (add/revoke grants) and transfer ownership. The owner holds it implicitly.
--
-- `revocable` distinguishes user-manageable grants (default true) from "fixed"
-- grants the switchboard inserts and then marks irrevocable -- e.g. when a user
-- starts a job on a group-owned host, the owning group is given an
-- irrevocable `stop` grant on the job, because the job consumes that group's
-- resources. Irrevocable rows cannot be deleted or modified by anyone (enforced
-- by trigger); they are cleared only when the resource itself is deleted
-- (FK cascade). The switchboard inserts a row as revocable, then UPDATEs
-- revocable -> false to lock it.
create type tml_switchboard.host_permission as enum ('read', 'start', 'ssh', 'manage');
create type tml_switchboard.job_permission as enum ('read', 'stop', 'ssh', 'manage');

create table tml_switchboard.host_grants
(
    host_id    uuid                              not null references tml_switchboard.hosts (host_id)         on delete cascade,
    subject_id uuid                              not null references tml_switchboard.subjects (subject_id) on delete cascade,
    permission tml_switchboard.host_permission   not null,
    revocable  bool                              not null default true,
    granted_at timestamp with time zone          not null default current_timestamp,

    primary key (host_id, subject_id, permission)
);

create table tml_switchboard.job_grants
(
    job_id     uuid                            not null references tml_switchboard.jobs (job_id)        on delete cascade,
    subject_id uuid                            not null references tml_switchboard.subjects (subject_id) on delete cascade,
    permission tml_switchboard.job_permission  not null,
    revocable  bool                            not null default true,
    granted_at timestamp with time zone        not null default current_timestamp,

    primary key (job_id, subject_id, permission)
);

-- Block deletion or modification of irrevocable (revocable = false) grants. A
-- CHECK constraint governs what rows may exist, not who may delete them, so this
-- guard must be a row-level trigger. It fires only on direct DELETE/UPDATE of a
-- grant row; FK-cascade teardown when the parent resource is deleted is not
-- gated by it, so resource deletion still cleans grants up. Setting revocable
-- false on a currently-revocable row is permitted -- that is how the switchboard
-- locks a freshly-inserted grant.
create or replace function tml_switchboard.deny_irrevocable_grant_change()
    returns trigger
    language plpgsql as
$$
begin
    if OLD.revocable = false then
        raise exception 'grant on %.% for % is irrevocable',
            TG_TABLE_SCHEMA, TG_TABLE_NAME, OLD.subject_id;
    end if;
    if TG_OP = 'DELETE' then
        return OLD;
    end if;
    return NEW;
end;
$$;

create trigger host_grants_irrevocable
    before delete or update on tml_switchboard.host_grants
    for each row execute function tml_switchboard.deny_irrevocable_grant_change();
create trigger job_grants_irrevocable
    before delete or update on tml_switchboard.job_grants
    for each row execute function tml_switchboard.deny_irrevocable_grant_change();

-- ===========================================================================
-- JOB PARAMETERS
-- ===========================================================================

create type tml_switchboard.parameter_value as
(
    value     text,
    is_secret bool
);

-- A table of its own because per-job parameter key uniqueness is a useful
-- property to enforce.
create table tml_switchboard.job_parameters
(
    job_id uuid                            not null references tml_switchboard.jobs (job_id) on delete cascade,
    key    text                            not null,
    value  tml_switchboard.parameter_value not null,

    primary key (job_id, key)
);

-- ===========================================================================
-- JOB TARGET REQUIREMENTS
-- ===========================================================================
--
-- The ordered array of targets (DUTs) a job requests. One row per requested
-- target; `req_index` is its position in the submitted array (so a job asking
-- for two identical DUTs is two rows). Each carries a required tag subset that
-- the assigned `host_targets` row must satisfy by containment. A job with no
-- rows requests no DUTs (e.g. a pure-VM job). The concrete DUT chosen for each
-- requirement is recorded at schedule time (a future `job_assigned_targets`
-- table, added with the scheduler).
create table tml_switchboard.job_target_requirements
(
    job_id    uuid   not null references tml_switchboard.jobs (job_id) on delete cascade,
    req_index int    not null,
    tags      text[] not null default '{}',

    primary key (job_id, req_index)
);

-- ===========================================================================
-- SCHEDULING
-- ===========================================================================
--
-- Coordination between the scheduler and the per-host supervisor workers is
-- DB-only (no in-process channels), so they can be distributed across processes;
-- for now both poll, and Postgres LISTEN/NOTIFY can replace polling later. See
-- doc/oci-image-migration-plan.md §8.3.

-- The scheduler scans queued jobs oldest-first; a partial index keeps that scan
-- cheap as the (mostly non-queued) jobs table grows.
create index jobs_queued_idx on tml_switchboard.jobs (queued_at)
    where job_state = 'queued';

-- Hosts a job may be dispatched onto, by the set-based criteria SQL expresses
-- well: the host is idle (no current job), live (its worker's heartbeat
-- `last_seen_at` is newer than the caller-supplied staleness cutoff), and
-- host-tag eligible (its opaque `tags` are a superset of the job's
-- `host_tag_requirements`). `tags @> '{}'` holds for every host, so a job with
-- no host-tag requirements matches all idle/live hosts.
--
-- This is only the cheap pre-filter: the scheduler additionally applies the
-- target/DUT bipartite match and the image resolution (neither of which belongs
-- in SQL) under a row lock in its dispatch transaction.
--
-- TODO(authz): this does NOT yet restrict to hosts the job's enqueuing principal
-- is permitted to use. Authorization (host ownership, or a `start` entry in
-- `host_grants`, evaluated via `principals()`) should be folded in here as an
-- additional join/EXISTS predicate so unauthorized hosts never become
-- candidates. Until then the scheduler may place a job on any tag-eligible host.
create function tml_switchboard.eligible_hosts(
    p_job_id          uuid,
    p_liveness_cutoff timestamp with time zone
)
    returns setof uuid
    language sql
    stable as
$$
    select h.host_id
    from tml_switchboard.hosts h
    where h.current_job is null
      and h.last_seen_at is not null
      and h.last_seen_at > p_liveness_cutoff
      and h.tags @> (
          select j.host_tag_requirements
          from tml_switchboard.jobs j
          where j.job_id = p_job_id
      )
    order by h.host_id;
$$;

-- ===========================================================================
-- IMAGE CATALOG
-- ===========================================================================
--
-- The switchboard is the catalog of OCI images and image groups (see
-- doc/oci-image-migration-plan.md §5.3/§8). It stores only references --
-- `{registry, repository}` locations and the content-addressed digest -- never
-- image bytes. An image's *identity* is its OCI manifest digest; the same digest
-- may be served from several locations (registry redundancy, promotion to a
-- canonical/system mirror), so locations are a separate table (D12/D16). A job
-- referencing an image by digest is resolved, at dispatch, to that digest plus
-- its ordered locations and handed to the supervisor.

-- A registered Treadmill image, identified by its OCI manifest digest. `attrs`
-- is free-form metadata projected off the validated manifest at registration
-- time; `artifact_type` records the manifest's `artifactType` for display.
create table tml_switchboard.images
(
    id                uuid                     not null primary key,
    manifest_digest   text                     not null unique,
    artifact_type     text                     not null,

    -- Owning subject (user or group). NULL means orphaned (cf. hosts/jobs): the
    -- resource survives its owner's deletion but is then admin-only.
    owner_subject     uuid                     references tml_switchboard.subjects (subject_id) on delete set null,

    label             text,
    attrs             jsonb                    not null default '{}'::jsonb,
    created_at        timestamp with time zone not null default current_timestamp
);

-- The registry locations a given image's bytes can be pulled from. `status`
-- distinguishes the user's original `external` registry from `canonical`/`system`
-- mirrors added by promotion (the digest never changes; promotion is an INSERT).
create table tml_switchboard.image_locations
(
    image_id          uuid                     not null references tml_switchboard.images (id) on delete cascade,
    registry          text                     not null,
    repository        text                     not null,
    status            text                     not null,
    added_at          timestamp with time zone not null default current_timestamp,

    primary key (image_id, registry, repository),

    constraint valid_location_status check (status in ('external', 'canonical', 'system'))
);

-- A registered Treadmill image group: an OCI image index, pinned by its index
-- digest. Its members are concrete `images` rows (in the same repository as the
-- index), denormalized into `image_group_members` for the dispatch-time matcher.
create table tml_switchboard.image_groups
(
    id                uuid                     not null primary key,
    index_digest      text                     not null unique,
    owner_subject     uuid                     references tml_switchboard.subjects (subject_id) on delete set null,
    label             text,
    created_at        timestamp with time zone not null default current_timestamp
);

-- One selectable member of a group, denormalized from the index for the matcher
-- (group + host tags -> concrete image). A member's eligibility is a set of
-- required host tags (authored per index member, see `parse_group`): the member
-- is admissible for a host iff `hosts.tags` is a superset of `required_host_tags`.
-- Among admissible members the most specific (largest required set) wins, ties
-- broken by `position` (the member's order in the index). Image selection uses
-- HOST tags only; target/DUT tags are irrelevant here. Rebuilt wholesale from the
-- index when the group is (re)registered.
create table tml_switchboard.image_group_members
(
    group_id            uuid                     not null references tml_switchboard.image_groups (id) on delete cascade,
    image_id            uuid                     not null references tml_switchboard.images (id) on delete cascade,
    required_host_tags  text[]                   not null default '{}',
    position            int                      not null,

    primary key (group_id, image_id)
);

-- ===========================================================================
-- AUDIT LOG
-- ===========================================================================
--
-- A curated, durable, append-only record of system state changes (and a few
-- observational events). Written in the SAME transaction as the change it
-- describes, via the `audit::transition`/`audit::emit` chokepoint in the Rust
-- code -- so a rolled-back state change never leaves an event behind, and an
-- event provably describes its mutation. See `AUDIT_LOG_PLAN.md` for the design.
--
-- Payloads are stored as raw structured JSON and rendered at view time by a
-- per-event-type renderer keyed off `event_type`; the read API redacts based on
-- the viewer's permissions on the related entity. Secret VALUES (e.g. the
-- contents of `parameter_value.is_secret` parameters) must never be embedded in
-- a payload -- store the key name and a redacted marker instead.

-- One row per state change or observational event. Append-only: a trigger blocks
-- UPDATE/DELETE in normal operation (see `deny_audit_event_change` below); the
-- relation cascade exists only for a future GC that deletes whole events
-- deliberately by temporarily bypassing the trigger.
--
-- `event_id` is a UUIDv7 minted by the application: time-ordered so the primary
-- key index is also the natural newest-first ordering for feed queries.
--
-- `actor_id` and `correlation_id` are PLAIN uuids with no foreign key. The
-- referenced subject/trace may be deleted or never have existed in this DB
-- (correlation ids come from `tracing`); the audit row must survive either way.
-- Worker-driven transitions with no human actor reference the well-known
-- `system` subject seeded above.
create table tml_switchboard.audit_events
(
    event_id       uuid                     not null primary key,
    event_type     text                     not null,
    payload        jsonb                    not null,
    actor_id       uuid                     not null,
    correlation_id uuid,
    created_at     timestamp with time zone not null default current_timestamp
);
create index audit_events_created_at_idx
    on tml_switchboard.audit_events (created_at);

-- The entity kinds an audit event can relate to. `subject` covers both users and
-- groups (matching the unified subjects model above); resource-scoped relations
-- use `job` or `host`.
create type tml_switchboard.audit_entity_kind as enum ('job', 'host', 'subject');
-- An event's role with respect to a related entity. `actor` is the principal
-- that caused it; `subject` is the entity acted upon; `context` is any
-- additional entity whose viewers should also be able to see the event (e.g.
-- the host on which a job ran).
create type tml_switchboard.audit_role as enum ('actor', 'subject', 'context');

-- Links an event to an entity it relates to, with the view policy for THIS
-- relation. `entity_id` is a PLAIN uuid with NO foreign key: that is what keeps
-- audit history alive after the referenced job/host/subject is deleted
-- (matching the `owner_id on delete set null` provenance rationale above).
--
-- `view_policy` is the EXACT permission a viewer must hold on `entity` to see
-- the event through this relation -- one of the host/job permission enum
-- values ('read'|'start'|'ssh'|'stop'|'manage') stored as text so the same
-- column covers both resource kinds, plus the sentinel 'operator_only' for
-- system-internal events that never surface in a user-facing feed. Visibility
-- is disjunctive across relations: a viewer sees the event if they satisfy ANY
-- one relation's policy. Permission-implication (e.g. `manage` => `read`) is a
-- deliberate non-goal; matching is exact.
create table tml_switchboard.audit_event_relations
(
    event_id    uuid                              not null
                  references tml_switchboard.audit_events (event_id) on delete cascade,
    entity_kind tml_switchboard.audit_entity_kind not null,
    entity_id   uuid                              not null,
    role        tml_switchboard.audit_role        not null,
    view_policy text                              not null,

    primary key (event_id, entity_kind, entity_id, role)
);
-- The feed's hot path: "events for entity (kind, id), newest first". UUIDv7
-- `event_id` is time-ordered, so a DESC scan on this index also yields
-- chronological order without a separate sort.
create index audit_event_relations_entity_idx
    on tml_switchboard.audit_event_relations (entity_kind, entity_id, event_id desc);

-- Enforce append-only on `audit_events`: any direct UPDATE or DELETE raises.
-- Same trigger pattern as `deny_irrevocable_grant_change` above. FK-cascade
-- teardown of `audit_event_relations` when an event is deleted is not gated by
-- this trigger (it fires on `audit_events`, not relations), so a deliberate GC
-- that drops the trigger to remove whole events still cleans the relation rows
-- up via the cascade.
create or replace function tml_switchboard.deny_audit_event_change()
    returns trigger
    language plpgsql as
$$
begin
    raise exception 'audit events are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;

create trigger audit_events_append_only
    before update or delete on tml_switchboard.audit_events
    for each row execute function tml_switchboard.deny_audit_event_change();
