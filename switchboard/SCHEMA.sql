-- -*- fill-column: 80; -*-
CREATE SCHEMA tml_switchboard;


-- =============================================================================
-- SUBJECTS: users and groups
-- =============================================================================
--
-- Users and groups are both "subjects": principals that can
--   1. own resources,
--   2. be granted permissions on resources, and
--   3. be members of groups.
--
-- To avoid scattering polymorphic (kind, id) pairs across every ownership,
-- membership, and grant table, both users and groups extend a single `subjects`
-- row. Every ownership/grant/membership FK in this schema therefore references
-- `subjects(subject_id)`, and "a user or a group" is expressible as one foreign
-- key.
--
-- `system` subjects are non-human service actors (the switchboard worker,
-- internal automation). `system` subjects may not be associated with a `user`
-- role (in which case the system resolves their IDs to some entity internally).
-- They are seeded out of band, through SQL migrations or by an administrator.
-- They can't be created / log in interactively.
CREATE TYPE tml_switchboard.subject_kind AS enum('user', 'group', 'system');


CREATE TABLE tml_switchboard.subjects (
    subject_id uuid NOT NULL PRIMARY KEY,
    kind tml_switchboard.subject_kind NOT NULL
);


-- A human (or system) principal.
--
-- - `username` is the internal Treadmill handle, suggested from the provider
--   login at first sign-in but freely changeable, hence its own uniqueness
--   independent of any provider identity.
-- - `locked` deactivates the account without deleting it (preserving job/audit
--   provenance). A locked account must not be able to login, and its tokens
--   must not be accepted.
CREATE TABLE tml_switchboard.users (
    subject_id uuid NOT NULL PRIMARY KEY REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    username text NOT NULL UNIQUE,
    full_name text,
    avatar_url text,
    locked bool NOT NULL DEFAULT FALSE,
    -- ToS acceptance. NULL version = never accepted. A user whose accepted
    -- version is below the configured current version must re-accept before a
    -- token is issued (their account is not otherwise gated).
    tos_accepted_version integer,
    tos_accepted_at timestamp with time zone
);


-- User <-> OAuth identity link.
--
-- - `provider_user_id`: a *stable* user identifier with the provier.
-- - `provider_login` current external handle for display only.
--
-- A user holds at most one identity per provider.
CREATE TABLE tml_switchboard.user_identities (
    provider text NOT NULL,
    provider_user_id text NOT NULL,
    user_id uuid NOT NULL REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    provider_login text,
    linked_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (provider, provider_user_id),
    UNIQUE (user_id, provider)
);


-- Verified email addresses, used to link a future provider login to an existing
-- user (same verified email => same person).
--
-- Globally unique so it can serve as that linking key, multi-row per user, and
-- tagged with the provider that originates from.
--
-- Only `verified` email addresses must be used for any sensitive flow: linking
-- on an unverified address would allow account takeover by claiming someone
-- else's email on the external provider.
CREATE TABLE tml_switchboard.user_emails (
    email text NOT NULL PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    provider text NOT NULL,
    verified bool NOT NULL,
    added_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


-- A group/organization. Owns resources and aggregates members like any subject.
CREATE TABLE tml_switchboard.groups (
    subject_id uuid NOT NULL PRIMARY KEY REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    name text NOT NULL UNIQUE
);


-- The system-global admin group.
--
-- Its UUID is a hard-coded well-known constant so it can be referenced directly
-- in authorization queries. Global admin authority = membership in this group;
-- orphaned resources (owner_id IS NULL) are manageable only by its members.
INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000001', 'group');


INSERT INTO
    tml_switchboard.groups (subject_id, name)
VALUES
    ('00000000-0000-0000-0000-000000000001', 'admins');


-- The system actor.
--
-- A `system` subject (non-human service principal) with a hard-coded well-known
-- UUID, so the switchboard can attribute the audit events it raises on its own
-- initiative -- worker-driven job lifecycle transitions, host assignment,
-- internal automation -- to a stable actor referenceable directly in code.
--
-- It is a bare `subjects` row with no `users`/`groups` extension: it never logs
-- in and owns nothing; it exists only to be named as the actor of
-- system-originated events.
INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000002', 'system');


-- The anonymous actor.
--
-- A `system` subject used as the audit `actor_id` for events raised about an
-- UNauthenticated external party who has no local subject -- e.g. a login
-- denied by the admission gate before any user record exists. Never logs in,
-- owns nothing.
INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000003', 'system');


-- The `everyone` subject: the public.
--
-- A `group` subject with a hard-coded well-known UUID that EVERY subject is
-- implicitly a member of -- `principals()` (below) always unions it in. Granting
-- it a permission on any resource therefore makes that permission public: it is
-- the single, uniform mechanism for "public" across every resource kind (hosts,
-- jobs, image sets, image sources), replacing the per-entity `public` boolean
-- columns. It owns nothing and never logs in; it exists only to be a grantee.
--
-- Because membership is implicit (not materialized in `group_members`), the
-- acyclicity trigger and manual membership management never see it.
INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000004', 'group');


INSERT INTO
    tml_switchboard.groups (subject_id, name)
VALUES
    (
        '00000000-0000-0000-0000-000000000004',
        'everyone'
    );


-- =============================================================================
-- GROUP MEMBERSHIP: a many-to-many DAG with provenance
-- =============================================================================
--
-- `member_id` references `subjects`, so a member may itself be a group -- this
-- is what gives nested groups. Membership is therefore a directed graph; we
-- enforce that it stays acyclic (see trigger below).
--
-- `source` records WHO added the membership: the same subject may be both a
-- manually-added member and an auto-group member, so `source` is part of the
-- primary key. Auto-sync (e.g. GitHub org reconciliation) only ever
-- inserts/deletes rows of its own source, so it can never clobber manual
-- memberships, and vice versa. `source_ref` carries the external key (e.g. the
-- GitHub org id) for auto sources and is part of the primary key too: a subject
-- may be in one group via several sources of the same kind (e.g. two GitHub
-- orgs that both feed it), and each must be an independent,
-- separately-reconciled row. It is NOT NULL (default '') so it participates in
-- the PK uniformly; manual rows simply carry the empty string.
CREATE TYPE tml_switchboard.membership_source AS enum('manual', 'github_org');


CREATE TABLE tml_switchboard.group_members (
    group_id uuid NOT NULL REFERENCES tml_switchboard.groups (subject_id) ON DELETE CASCADE,
    member_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    source tml_switchboard.membership_source NOT NULL,
    source_ref text NOT NULL DEFAULT '',
    PRIMARY KEY (group_id, member_id, source, source_ref),
    -- Trivial self-cycle is a cheap declarative constraint; longer cycles need
    -- the reachability trigger below.
    CONSTRAINT no_self_membership CHECK (group_id <> member_id)
);


-- Upward traversal (member -> the groups it belongs to) hits `member_id`; the PK
-- already indexes the `group_id` prefix for downward traversal.
CREATE INDEX group_members_member_id_idx ON tml_switchboard.group_members (member_id);


-- Enforce that the membership graph stays acyclic. Adding the edge "group_id
-- contains member_id" closes a cycle iff member_id can already reach group_id
-- by following existing "contains" edges. Updates are rare and traversal is
-- hot, so paying a reachability check on write only is the right trade.
--
-- Correctness under concurrency: two transactions could each add an edge that
-- is individually acyclic but jointly cyclic, neither seeing the other's
-- uncommitted row. We close that hole *inside the trigger* with a
-- transaction-scoped advisory lock held until commit. It does not conflict with
-- the INSERT's ROW EXCLUSIVE table lock (so no deadlock), and it serializes all
-- membership mutations: a waiter resumes only after the holder commits, and
-- under READ COMMITTED its fresh per-statement snapshot then includes that
-- committed edge -- so any attempt that would close a cycle sees it and aborts.
-- (Assumes READ COMMITTED, the default. Under higher isolation, use
-- SERIALIZABLE instead, whose SSI detects the same conflict and aborts one
-- transaction.)
CREATE OR REPLACE FUNCTION tml_switchboard.group_members_no_cycle () returns trigger language plpgsql AS $$
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
CREATE CONSTRAINT TRIGGER group_members_acyclic
AFTER insert
OR
UPDATE ON tml_switchboard.group_members FOR each ROW
EXECUTE function tml_switchboard.group_members_no_cycle ();


-- Declares that a group's membership is auto-synced from an external source
-- (e.g. a GitHub organization). The reconciler iterates these bindings --
-- lazily on a member's re-auth and periodically -- and for each one
-- inserts/deletes `group_members` rows with `source = 'github_org'` and
-- `source_ref = external_id`, touching only auto rows so manual memberships are
-- preserved.
--
-- `external_id` is the provider's stable identifier for the source (e.g. the
-- GitHub org's numeric id), not its renameable login; `external_name` is kept
-- for display only. A group may sync from several sources, and one source may
-- feed several groups, hence the composite key. `last_synced_at` is null until
-- the first successful reconciliation.
CREATE TABLE tml_switchboard.group_auto_sources (
    group_id uuid NOT NULL REFERENCES tml_switchboard.groups (subject_id) ON DELETE CASCADE,
    provider text NOT NULL,
    external_id text NOT NULL,
    external_name text,
    membership_via tml_switchboard.membership_source NOT NULL,
    last_synced_at timestamp with time zone,
    PRIMARY KEY (group_id, provider, external_id)
);


-- =============================================================================
-- API TOKENS
-- =============================================================================
CREATE TYPE tml_switchboard.api_token_revocation AS (
    revoked_at timestamp with time zone,
    revocation_reason text
);


-- A token currently acts with the full authority of its owning subject;
-- per-token scoping can be added later as a `token_grants` table without
-- disturbing this one.
--
-- Tokens have both natural expiration and an explicit revocation mechanism,
-- which voids a token before it expires.
--
-- `user_agent` and `created_ip`/`created_port` record the provenance of a token
-- at mint time (the client that requested it), surfaced in the session-list API
-- and mirrored into the `session_token_issued` audit event. `comment` is an
-- optional user-supplied label.
CREATE TABLE tml_switchboard.api_tokens (
    token_id uuid NOT NULL PRIMARY KEY,
    token bytea NOT NULL UNIQUE,
    user_id uuid NOT NULL REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    revoked tml_switchboard.api_token_revocation,
    created_at timestamp with time zone NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    user_agent text,
    comment text,
    -- `created_{ip,port}` nullable, for when a token is minted manually by an
    -- operator. In that case, the `user_agent` should be populated
    -- appropriately, and an audit event inserted manually.
    created_ip text,
    created_port integer,
    CHECK (octet_length(token) = 32)
);


-- =============================================================================
-- OAUTH FLOWS
-- =============================================================================
--
-- Short-lived server-side record of an in-flight OAuth authorization-code flow.
-- When the switchboard redirects a user to the provider it mints a random CSRF
-- `state` and persists it here; on callback it looks the state up (consuming
-- the row) to confirm the response corresponds to a flow this server actually
-- initiated. `provider` records which provider the flow targets so the callback
-- knows which token endpoint and identity mapping to use. Rows are deleted on
-- successful callback and can be swept after `expires_at`; a never-completed
-- flow simply expires.
CREATE TABLE tml_switchboard.oauth_flows (
    state text NOT NULL PRIMARY KEY,
    provider text NOT NULL,
    pkce_verifier text,
    -- Where the callback sends the browser (with the single-use staged pair in
    -- the query) for a flow a browser frontend initiated. Validated against the
    -- configured allowlist when the flow starts; NULL for programmatic flows,
    -- which receive JSON from the callback instead.
    return_to text,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    expires_at timestamp with time zone NOT NULL
);


-- =============================================================================
-- LOGIN ADMISSION
-- =============================================================================
--
-- The admission allow-list: who may create a new local account via interactive
-- OAuth login. Consulted ONLY on the new-user path (see routes/auth.rs); existing
-- users are never gated. Hand-managed by operators via SQL; the switchboard only
-- ever reads it. Two entry kinds in one table:
--   kind='user' -> external_id is a provider_user_id (a specific identity)
--   kind='org'  -> external_id is a provider org id (any current member admitted)
-- Orthogonal to group_auto_sources (grouping/grants); this governs admission only.
CREATE TABLE tml_switchboard.login_allowlist (
    provider text NOT NULL,
    kind text NOT NULL,
    external_id text NOT NULL,
    comment text,
    added_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (provider, kind, external_id),
    CONSTRAINT valid_allowlist_kind CHECK (kind IN ('user', 'org'))
);


-- Short-lived server-side staging for a login whose identity the callback has
-- verified (e.g., token exchanged with the OAuth provider). EVERY interactive
-- login stages: `POST /auth/login/complete` is the sole point that mints a
-- session token, by consuming this row. A login may still require completion
-- steps (ToS acceptance) before it can be claimed, or be immediately claimable.
--
-- Consume-once: deleted in the same txn that provisions the user (new) or
-- records login (existing). Holds the derived identity and org list--the OAuth
-- access token is already discarded at this point and never written to the DB.
--
-- identity IS NOT NULL -> a brand-new user awaiting first acceptance
-- existing_user_id IS NOT NULL -> an existing user (re-accepting a bumped ToS,
--                                 or ready to claim)
--
-- To exchange a staged login (with the required additional information such as
-- ToS acceptance) to a proper auth token, the user has to present the `secret`
-- which hashes to `secret_hash`.
CREATE TABLE tml_switchboard.staged_logins (
    id uuid NOT NULL PRIMARY KEY,
    -- Salted argon2id hash (PHC string) of the one-time completion secret that
    -- accompanies the id across the browser round trip.
    secret_hash text NOT NULL,
    provider text NOT NULL,
    identity jsonb,
    existing_user_id uuid REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    org_ids TEXT[] NOT NULL DEFAULT '{}',
    -- Inherited from the initiating flow (see oauth_flows.return_to): where a
    -- browser-form completion sends the browser next. NULL for programmatic
    -- flows.
    return_to text,
    -- The browser's context, captured at the OAuth callback, so the session
    -- token minted later at completion is stamped with the logging-in client's
    -- address/agent rather than the (server-side) completing caller's.
    user_agent text,
    created_ip text,
    created_port integer,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    expires_at timestamp with time zone NOT NULL,
    CONSTRAINT staged_kind CHECK ((identity IS NULL) <> (existing_user_id IS NULL))
);


-- =============================================================================
-- HOSTS
-- =============================================================================
--
-- Host + port tuple for ssh_endpoints of a host.
CREATE DOMAIN tml_switchboard.port AS integer CHECK (
    value >= 0
    AND value < 65535
);


CREATE DOMAIN tml_switchboard.ssh_host AS text CHECK (value IS NOT NULL);


CREATE DOMAIN tml_switchboard.ssh_port AS tml_switchboard.port CHECK (value IS NOT NULL);


CREATE TYPE tml_switchboard.ssh_endpoint AS (
    ssh_host tml_switchboard.ssh_host,
    ssh_port tml_switchboard.ssh_port
);


-- A host is the managed device that actually runs the user workload; the
-- supervisor is the software process that drives it and is the WebSocket peer
-- of the switchboard.
--
-- The `hosts` table carries both the host's user-facing properties (name, tags,
-- ssh_endpoints, owner) and the credentials/state of the supervisor authorized
-- to drive it (auth_token, worker_instance_id).
--
-- The auth_token field authenticates the host's supervisor: a 32-byte random
-- string uniquely identifying the supervisor on connect.
CREATE TABLE tml_switchboard.hosts (
    host_id uuid NOT NULL PRIMARY KEY,
    name text NOT NULL,
    auth_token bytea NOT NULL UNIQUE,
    -- Host-specific tags, separate from target (DUT) tags.
    tags TEXT[] NOT NULL,
    -- Owning subject (user or group).
    --
    -- NULL means orphaned: the resource has no owner and is manageable only by
    -- members of the admin group. Resources are orphaned (not cascade-deleted)
    -- when their owner is deleted, hence `on delete set null`.
    owner_id uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    -- SSH endpoints that all jobs executing on this host are reachable under.
    -- Each endpoint is a "hostname:port" tuple. IPv6 addreses are enclosed in
    -- square brackets, such as "[::1]:22".
    --
    -- TODO: in the future, SSH endpoints & proxies may be dynamically assigned.
    -- In that case, this column would be removed, and the assigned endpoints
    -- will only be written to the `jobs` table.
    ssh_endpoints tml_switchboard.ssh_endpoint[] NOT NULL,
    -- A host can either be idle or have a job assigned. This column keeps track
    -- of this state. If a job is assigned, then the host is not idle, and the
    -- "sub-state" is determined through the job's state data.
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
    --
    -- Note: a host can be assigned a job that is already finalized. That is OK,
    -- and intended, as the supervisor WS worker will explicitly ask a
    -- supervisor to drop a given job. Only when the supervisor reports that it
    -- no longer holds a job will this row be cleared, and the host becomes idle
    -- and eligible for scheduling new jobs.
    current_job uuid,
    -- Each host is driven by at most one supervisor connection, serviced by a
    -- worker. To prevent multiple concurrent workers claiming ownership of the
    -- same host, each host--worker combination is assigned a unique,
    -- monotonically increasing "worker instance ID", checked by subsequent DB
    -- operations.
    worker_instance_id bigint NOT NULL DEFAULT 0,
    -- Liveness heartbeat: the current worker refreshes this on each periodic
    -- ping response, and clears it to NULL when it disconnects cleanly (without
    -- having been superseded by another WS worker).
    --
    -- A host is "live" (eligible for the scheduler to dispatch onto) iff
    -- `last_seen_at` is non-null and recent (within a separately configured
    -- staleness window).
    --
    -- - NULL -> no connected supervisor (although non-NULL does not mean a
    --   supervisor is connected).
    --
    -- - A stale timestamp -> the WS worker died silently or the supervisor has
    --   not responsed to a ping within that time.
    --
    -- The scheduler uses this field when filtering for supervisors eligible for
    -- scheduling a given job.
    last_seen_at timestamp with time zone,
    CHECK (octet_length(auth_token) = 32),
    CHECK (worker_instance_id >= 0)
);


-- =============================================================================
-- JOBS
-- =============================================================================
--
-- Restart policy dictates the number of times a job can be restarted
-- automatically by the switchboard. Currently just a primitive counter (in the
-- future, the policy may become more expressive). The switchboard may
-- internally expose an upper bound on this, or require special permissions to
-- set a given restart count).
CREATE TYPE tml_switchboard.restart_policy AS (remaining_restart_count integer);


-- The job's execution lifecycle state.
CREATE TYPE tml_switchboard.job_state AS enum(
    -- Accepted, no host assigned yet; eligible for scheduling.
    'queued',
    -- Bound to a host but StartJob not yet sent; no longer eligible for
    -- (re)scheduling. dispatched_on_host_id set, started_at null.
    'assigned',
    -- Assigned & starting up; sub-states in `initializing_stage`.
    'initializing',
    -- Assigned & running.
    'ready',
    -- Assigned & shutting down.
    'terminating',
    -- Terminal; `termination_reason` and `terminated_at` are populated.
    'finalized'
);


-- The sub-stage of an `initializing` job, mirroring the wire-level
-- `JobInitializingStage`. Not intended to be used/interpreted in the
-- switchboard, just informational for the user.
CREATE TYPE tml_switchboard.job_initializing_stage AS enum(
    'starting',
    'fetching_image',
    'allocating',
    'provisioning',
    'booting'
);


-- Why a job terminated. Set once, at finalization. Orthogonal to the task's
-- exit status (success/failure of the user workload) and to any exit message.
CREATE TYPE tml_switchboard.termination_reason AS enum(
    -- workload-driven (the job's own process ended it)
    'workload_exited', -- e.g., QEMU process exits
    'workload_self_terminated', -- e.g., termination requested with puppet
    -- externally terminated
    'user_terminated',
    -- timeouts
    'queue_timeout',
    'execution_timeout',
    -- bad / unfetchable image
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
-- relayed by the host's supervisor.
--
-- Set out-of-band of termination and revisable while assigned: `pending` until
-- known, then `success`/`failure`. Once set it is never cleared. Orthogonal to
-- termination_reason.
CREATE TYPE tml_switchboard.task_exit_status AS enum('pending', 'success', 'failure');


CREATE TABLE tml_switchboard.jobs (
    job_id uuid NOT NULL PRIMARY KEY,
    -- Owning subject (user or group).
    --
    -- NULL means orphaned (see hosts).
    --
    -- For a job started by a user on a host owned by another subject (e.g.,
    -- other user that has granted access, or group that the useris a member
    -- of), the enqueueing user is the owner; the host owner's control over the
    -- job is conferred separately, as an irrevocable grant inserted at dispatch
    -- time (see job_grants).
    owner_id uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    -- These columns specify the job image and the nature of the job. A job's
    -- image is referenced against the switchboard image catalog: either a
    -- concrete image (`image_id`, a registered `images` row) or an image set
    -- (`image_set_id` plus the frozen `image_set_generation`, an
    -- `image_set_generations` row) resolved to a concrete member at dispatch.
    --
    --  (1) normal job:    exactly one of image_id / image_set_id is set; both
    --                     resume_job_id and restart_job_id are null
    --
    --  (2) restarted job: exactly one of image_id / image_set_id is set;
    --                     restart_job_id is set
    --
    --  (3) resumed job:   resume_job_id is set; image_id / image_set_id null
    --
    -- `image_set_generation` is set exactly when `image_set_id` is: the
    -- generation is frozen at enqueue so the candidate set is reproducible. The
    -- FKs to `images` / `image_set_generations` are added by ALTER TABLE
    -- after the IMAGE CATALOG section, since those tables are defined later in
    -- this file (same forward-reference pattern as `hosts.current_job`).
    resume_job_id uuid REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION,
    restart_job_id uuid REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION,
    image_id uuid,
    image_set_id uuid,
    image_set_generation int,
    -- The concrete image actually dispatched, recorded at dispatch for
    -- reproducibility/audit (the digest is recovered by join to `images`).
    --
    -- For a concrete-image job this equals `image_id`; for an image-set job it
    -- is the member the matcher selected.
    resolved_image_id uuid,
    -- SSH keys to be injected into the job, if any. Can be updated throughout
    -- the job's lifetime, new keys get deployed through the puppet.
    ssh_keys TEXT[] NOT NULL,
    -- Job's restart policy.
    restart_policy tml_switchboard.restart_policy NOT NULL,
    -- Token the job was enqueued by. Used to determine which hosts a job can
    -- run on, based on the subject that the token belongs to.
    enqueued_by_token_id uuid NOT NULL REFERENCES tml_switchboard.api_tokens (token_id) ON DELETE NO ACTION,
    -- Host eligibility: the set of tags a host must carry (as a superset) for
    -- this job to be schedulable onto it. Opaque strings, matched by
    -- containment against `hosts.tags`. Target (DUT) requirements live in the
    -- separate `job_target_requirements` table.
    host_tag_requirements TEXT[] NOT NULL DEFAULT '{}',
    -- The amount of time the job can run before it is killed.
    job_timeout interval NOT NULL,
    -- The latest known execution-lifecycle state of the job.
    job_state tml_switchboard.job_state NOT NULL,
    -- The sub-stage while `job_state = 'initializing'`; null otherwise.
    initializing_stage tml_switchboard.job_initializing_stage,
    -- The time at which the job was queued. If after
    -- [config:api.jobs.queue_timeout] the job is still queued, it is
    -- terminated.
    queued_at timestamp with time zone NOT NULL,
    -- The time at which the job was started.
    started_at timestamp with time zone,
    -- ID of the host the job was dispatched to.
    dispatched_on_host_id uuid,
    -- SSH endpoints that this job is reachable under, populated once assigned
    -- to a host that exposes endpoints.
    ssh_endpoints tml_switchboard.ssh_endpoint[],
    -- User-requested termination signal: the DB side of user-terminate.
    --
    -- When set, the assigned job's worker converges the job to `finalized` with
    -- `termination_reason = user_terminated`. It is re-read on every reconcile
    -- pass (alongside the execution-timeout deadline), so the decision is
    -- always made against fresh state and composes with a later API that sets
    -- this column.
    --
    -- NULL means no termination requested. Queued (unassigned) jobs are the
    -- scheduler/reaper's responsibility, not a worker's.
    terminate_requested_at timestamp with time zone,
    -- Filled out when transitioned into `finalized`.
    --
    -- `termination_reason` records *why* the job stopped; `task_exit_status`
    -- records the *result of the user workload* (independent of the reason);
    -- `exit_message` is an optional human-readable note. Captured workload
    -- output is stored in object storage, not here.
    termination_reason tml_switchboard.termination_reason,
    task_exit_status tml_switchboard.task_exit_status,
    exit_message text,
    terminated_at timestamp with time zone,
    ---->> INVARIANT CHECKING <<----
    -- Two allowed init states:
    --  (1) resume_job_id = null, restart_job_id = _, and exactly one of
    --      image_id / image_set_id is set; image_set_generation is set iff
    --      image_set_id is
    --  (2) resume_job_id != null, restart_job_id = null, and image_id /
    --      image_set_id / image_set_generation all null
    CONSTRAINT valid_init_spec CHECK (
        (
            resume_job_id IS NULL
            AND (image_id IS NOT NULL)::int + (image_set_id IS NOT NULL)::int = 1
            AND (image_set_id IS NULL) = (image_set_generation IS NULL)
        )
        OR (
            resume_job_id IS NOT NULL
            AND restart_job_id IS NULL
            AND image_id IS NULL
            AND image_set_id IS NULL
            AND image_set_generation IS NULL
        )
    ),
    -- Restart count >= 0
    CONSTRAINT valid_restart_policy CHECK ((restart_policy).remaining_restart_count >= 0),
    -- A host is bound from `assigned` onwards (through `finalized`);
    -- `dispatched_on_host_id` tracks that binding for the assigned,
    -- not-yet-terminal lifecycle states.
    CONSTRAINT dispatched_host_iso_assigned CHECK (
        (dispatched_on_host_id IS NOT NULL) = (
            job_state IN (
                'assigned',
                'initializing',
                'ready',
                'terminating'
            )
        )
    ),
    -- `started_at` is set exactly while executing, i.e. once dispatched
    -- (`initializing`) and until terminal.
    CONSTRAINT started_at_iso_executing CHECK (
        (started_at IS NOT NULL) = (
            job_state IN ('initializing', 'ready', 'terminating')
        )
    ),
    -- The terminal record is set exactly when `finalized`.
    CONSTRAINT termination_reason_iso_finalized CHECK (
        (termination_reason IS NOT NULL) = (job_state = 'finalized')
    ),
    CONSTRAINT terminated_at_iso_finalized CHECK (
        (terminated_at IS NOT NULL) = (job_state = 'finalized')
    ),
    -- `task_exit_status` is absent while `queued` (no host assigned yet) and
    -- may be present in any later state.
    CONSTRAINT task_exit_status_absent_while_queued CHECK (
        task_exit_status IS NULL
        OR job_state <> 'queued'
    ),
    -- `initializing_stage` is present exactly while `initializing`.
    CONSTRAINT initializing_stage_iso_initializing CHECK (
        (initializing_stage IS NOT NULL) = (job_state = 'initializing')
    )
);


ALTER TABLE tml_switchboard.hosts
ADD FOREIGN KEY (current_job) REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION;


-- A job may be the `current_job` of at most one host. Partial (only over
-- non-null `current_job`) so idle hosts don't collide on NULL.
CREATE UNIQUE INDEX hosts_current_job_unique ON tml_switchboard.hosts (current_job)
WHERE
    current_job IS NOT NULL;


-- Host tags are opaque strings (`key=value` pairs or bare flags, by convention
-- only -- the matcher never parses them). A job requests a host whose tags are
-- a superset of the job's `host_tag_requirements`; the GIN index backs that
-- containment (`tags @> required`) lookup over the host pool.
CREATE INDEX hosts_tags_gin ON tml_switchboard.hosts USING gin (tags);


-- =============================================================================
-- HOST TARGETS (DUTs)
-- =============================================================================
--
-- A host drives one or more attached targets (devices under test). Each target
-- carries its own opaque tag set (e.g. `board=nrf52840dk`, `ble`, `gpio`),
-- independent of the host's tags. A job requests an array of targets, each by a
-- required tag subset; the scheduler assigns each requested target to a
-- distinct DUT whose tags satisfy it (a bipartite match, done in the
-- application). Target tags do NOT participate in image selection -- that is
-- host-tag-only (see `image_set_members`).
CREATE TABLE tml_switchboard.host_targets (
    target_id uuid NOT NULL PRIMARY KEY,
    host_id uuid NOT NULL REFERENCES tml_switchboard.hosts (host_id) ON DELETE CASCADE,
    -- Stable per-host label for the DUT (e.g. "dut0").
    name text NOT NULL,
    -- Opaque tag set, same convention as host tags.
    tags TEXT[] NOT NULL DEFAULT '{}',
    UNIQUE (host_id, name)
);


-- Backs the per-target containment (`tags @> required`) match during
-- scheduling.
CREATE INDEX host_targets_tags_gin ON tml_switchboard.host_targets USING gin (tags);


-- =============================================================================
-- PERMISSIONS / ACLs
-- =============================================================================
--
-- Authorization is ownership + explicit grants. Each resource carries an
-- `owner_id` (above); the grant tables below record additional (subject,
-- permission) entries. A subject is authorized for a permission on a resource
-- if it (or any group it transitively belongs to) owns the resource or holds a
-- matching grant -- the *policy* disjunction is evaluated in application SQL,
-- not here. The one shared building block both sides of that disjunction need
-- -- the set of a subject's transitive group memberships -- is the `principals`
-- function below, so the application query stays free of the recursive CTE.
--
-- `principals(arg_subject)` returns the subject itself, every group it reaches
-- by following `member_id -> group_id` edges, and the well-known `everyone`
-- subject that every subject implicitly belongs to. It is a parameterized
-- function rather than a view so it is seeded with a single subject and walks
-- only that subject's memberships via `group_members_member_id_idx`, instead of
-- materializing the closure for every subject. Ownership and grant checks are
-- tested against this whole set (see `src/auth/engine.rs`).
--
-- Folding `everyone` in here is what makes a grant to the `everyone` subject
-- behave as "public": every access check runs against `principals()`, so a
-- resource that grants `everyone` a permission grants it to all. Ownership joins
-- compare against a resource's `owner_id`, which is never the `everyone`
-- subject, so this does not make anyone an implicit owner -- only grants widen.
CREATE OR REPLACE FUNCTION tml_switchboard.principals (arg_subject uuid) returns TABLE (id uuid) language sql stable AS $$
    with recursive reached(id) as (
        select arg_subject
        union
        select gm.group_id
        from tml_switchboard.group_members gm
        join reached r on gm.member_id = r.id
    )
    select id from reached
    union
    -- The `everyone` subject: implicit membership for every principal.
    select '00000000-0000-0000-0000-000000000004'::uuid;
$$;


-- `manage` is the meta-permission: holding it lets a subject edit the
-- resource's ACL (add/revoke grants) and transfer ownership. The owner holds it
-- implicitly.
--
-- `revocable` distinguishes user-manageable grants (default true) from "fixed"
-- grants the switchboard inserts and then marks irrevocable -- e.g. when a user
-- starts a job on a host owned by a group the user is member of, or owned by a
-- subject that has given the user a grant, the host-owning subjet is given an
-- irrevocable `stop` grant on the job, because the job consumes that owner's
-- resources. Irrevocable rows cannot be deleted or modified by anyone (enforced
-- by trigger); they are cleared only when the resource itself is deleted (FK
-- cascade).
CREATE TYPE tml_switchboard.host_permission AS enum('read', 'start', 'ssh', 'manage');


CREATE TYPE tml_switchboard.job_permission AS enum('read', 'stop', 'ssh', 'manage');


CREATE TABLE tml_switchboard.host_grants (
    host_id uuid NOT NULL REFERENCES tml_switchboard.hosts (host_id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    permission tml_switchboard.host_permission NOT NULL,
    revocable bool NOT NULL DEFAULT TRUE,
    granted_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (host_id, subject_id, permission)
);


CREATE TABLE tml_switchboard.job_grants (
    job_id uuid NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    permission tml_switchboard.job_permission NOT NULL,
    revocable bool NOT NULL DEFAULT TRUE,
    granted_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (job_id, subject_id, permission)
);


-- Block deletion or modification of irrevocable (revocable = false) grants. A
-- CHECK constraint governs what rows may exist, not who may delete them, so this
-- guard must be a row-level trigger. It fires only on direct DELETE/UPDATE of a
-- grant row; FK-cascade teardown when the parent resource is deleted is not
-- gated by it, so resource deletion still cleans grants up. Setting revocable
-- false on a currently-revocable row is permitted -- that is how the switchboard
-- locks a freshly-inserted grant.
CREATE OR REPLACE FUNCTION tml_switchboard.deny_irrevocable_grant_change () returns trigger language plpgsql AS $$
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


CREATE TRIGGER host_grants_irrevocable before delete
OR
UPDATE ON tml_switchboard.host_grants FOR each ROW
EXECUTE function tml_switchboard.deny_irrevocable_grant_change ();


CREATE TRIGGER job_grants_irrevocable before delete
OR
UPDATE ON tml_switchboard.job_grants FOR each ROW
EXECUTE function tml_switchboard.deny_irrevocable_grant_change ();


-- =============================================================================
-- JOB PARAMETERS
-- =============================================================================
--
CREATE TYPE tml_switchboard.parameter_value AS (value text, is_secret bool);


-- A table of its own because per-job parameter key uniqueness is a useful
-- property to enforce.
CREATE TABLE tml_switchboard.job_parameters (
    job_id uuid NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE CASCADE,
    key text NOT NULL,
    value tml_switchboard.parameter_value NOT NULL,
    PRIMARY KEY (job_id, key)
);


-- =============================================================================
-- JOB TARGET REQUIREMENTS
-- =============================================================================
--
-- The ordered array of targets (DUTs) a job requests. One row per requested
-- target; `req_index` is its position in the submitted array (so a job asking
-- for two identical DUTs is two rows). Each carries a required tag subset that
-- the assigned `host_targets` row must satisfy by containment. A job with no
-- rows requests no DUTs (e.g. a pure-VM job). The concrete DUT chosen for each
-- requirement is recorded at schedule time (a future `job_assigned_targets`
-- table, added with the scheduler).
CREATE TABLE tml_switchboard.job_target_requirements (
    job_id uuid NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE CASCADE,
    req_index int NOT NULL,
    tags TEXT[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (job_id, req_index)
);


-- =============================================================================
-- SCHEDULING
-- =============================================================================
--
-- Coordination between the scheduler and the per-host supervisor workers is
-- DB-only (no in-process channels)
--
-- The scheduler scans queued jobs oldest-first; a partial index keeps that scan
-- cheap as the (mostly non-queued) jobs table grows.
CREATE INDEX jobs_queued_idx ON tml_switchboard.jobs (queued_at)
WHERE
    job_state = 'queued';


-- `GET /jobs` lists readable jobs newest-first with keyset pagination on
-- `(queued_at, job_id)`. This index (matching that order) turns the listing
-- into an index range scan and makes the keyset seek depth-independent, across
-- all job states (unlike the partial index above).
CREATE INDEX jobs_queued_at_job_id_idx ON tml_switchboard.jobs (queued_at DESC, job_id DESC);


-- Hosts a job may be dispatched onto, by the set-based criteria SQL expresses:
-- the host is idle (no current job), live (its worker's heartbeat
-- `last_seen_at` is newer than the caller-supplied staleness cutoff),
-- host-tag eligible (its opaque `tags` are a superset of the job's
-- `host_tag_requirements`; `tags @> '{}'` holds for every host, so a job with
-- no host-tag requirements matches all idle/live hosts), and -- crucially --
-- one the job's owner is *authorized* to start on.
--
-- Authorization mirrors `can_access_host(owner, host, 'start')` in
-- `src/auth/engine.rs`: the job's owner (evaluated over its transitive
-- `principals()` set) is a global admin, owns the host, or holds a `start`
-- grant on it. Folding it in here makes the candidate set itself the
-- authoritative gate -- an unauthorized host never becomes a scheduling
-- candidate -- so the enqueue route need not (and does not) pre-authorize a
-- host at submit time. A job whose `owner_id` is NULL (orphaned, e.g. its owner
-- was deleted while it sat queued) matches no host and so is never scheduled;
-- like a tag-unschedulable job, it ages out via the queue timeout.
--
-- This is still only the cheap pre-filter: the scheduler additionally applies
-- the target/DUT bipartite match and the image resolution under a row lock in
-- its dispatch transaction, and re-checks this authorization there (host
-- ownership/grants can change between this scan and the lock).
CREATE FUNCTION tml_switchboard.eligible_hosts (
    p_job_id uuid,
    p_liveness_cutoff timestamp with time zone
) returns setof uuid language sql stable AS $$
    with job as (
        select owner_id, host_tag_requirements
        from tml_switchboard.jobs
        where job_id = p_job_id
    ),
    -- The job owner's transitive principals (owner + every group it reaches),
    -- the set every host authorization check below is tested against. NULL owner
    -- yields a single NULL principal, which matches nothing.
    owner_principals (id) as (
        select p.id
        from job, tml_switchboard.principals(job.owner_id) p
    )
    select h.host_id
    from tml_switchboard.hosts h
    where h.current_job is null
      and h.last_seen_at is not null
      and h.last_seen_at > p_liveness_cutoff
      and h.tags @> (select host_tag_requirements from job)
      and (
          -- The owner (or a group it reaches) is a global admin.
          exists (
              select 1 from owner_principals
              -- The seeded `admins` group; see `ADMINS_GROUP_ID` in engine.rs.
              where id = '00000000-0000-0000-0000-000000000001'
          )
          -- The owner (via principals) owns the host.
          or exists (
              select 1 from owner_principals op where op.id = h.owner_id
          )
          -- The owner (via principals) holds a `start` grant on the host.
          or exists (
              select 1
              from tml_switchboard.host_grants g
              join owner_principals op on g.subject_id = op.id
              where g.host_id = h.host_id and g.permission = 'start'
          )
      )
    order by h.host_id;
$$;


-- ===========================================================================
-- IMAGE CATALOG
-- ===========================================================================
--
-- The switchboard maintains catalog of Treadmill OCI images.
--
-- An image's *identity* is its OCI manifest digest; the same digest may be
-- served from several sources, so sources are a separate table. A job
-- referencing an image by id is resolved, at dispatch, to its digest plus the
-- ordered sources its owner may use, and handed to the supervisor.
--
-- Image *sets* are mutable, named, generationed entities.
--
-- A registered Treadmill image is a non-owned manifest identity: a content hash
-- with no owner and no ACL. `manifest_digest` is globally UNIQUE (the same
-- bytes are the same row for everyone). `title` is a cached projection of the
-- validated manifest, NOT user-writeable metadata; `artifact_type` records the
-- manifest's `artifactType` for display. Ownership and grantability live on the
-- image *sources* below, not here.
CREATE TABLE tml_switchboard.images (
    id uuid NOT NULL PRIMARY KEY,
    manifest_digest text NOT NULL UNIQUE,
    artifact_type text NOT NULL,
    title text,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


-- The registry sources a given image's bytes can be pulled from -- the ownable,
-- grantable, deletable entity of the image catalog (an image itself is not
-- owned; see above). `status` distinguishes the user's original `external`
-- registry from `canonical`/`system` mirrors added by promotion (the digest
-- never changes; promotion is an INSERT).
--
-- We use `canonical` / `system` to track the expected reliability/longevity of
-- the image and/or mirror. A `system` mirror is assumed to be under the control
-- of the Treadmill system administrators and always available. Certain images
-- may be said to live in `canonical` mirrors, which are assumed to be highly
-- available as well, although outside of the system's control.
--
-- `owner_subject` owns the source (add/delete/manage rights); NULL means
-- orphaned (cf. hosts/jobs): the source survives its owner's deletion but is
-- then admin-only. Whether a subject may *use* a source is the standard
-- ownership ∨ grant disjunction over `principals()` (see `image_source_grants`
-- and `image_source_usable()`). A public, unauthenticated source is one that
-- grants the `everyone` subject `use`.
--
-- A source may LATER carry credentials for a private registry (stored in an
-- external secret system or encrypted at rest), restricting `use` to grantees.
-- That is out of scope now: every source is public/unauthenticated.
CREATE TABLE tml_switchboard.image_sources (
    id uuid NOT NULL PRIMARY KEY,
    image_id uuid NOT NULL REFERENCES tml_switchboard.images (id) ON DELETE CASCADE,
    registry text NOT NULL,
    repository text NOT NULL,
    status text NOT NULL,
    owner_subject uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    added_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    UNIQUE (image_id, registry, repository),
    CONSTRAINT valid_location_status CHECK (status IN ('external', 'canonical', 'system'))
);


-- Image-source permissions.
CREATE TYPE tml_switchboard.image_source_permission AS enum('use', 'manage');


CREATE TABLE tml_switchboard.image_source_grants (
    source_id uuid NOT NULL REFERENCES tml_switchboard.image_sources (id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    permission tml_switchboard.image_source_permission NOT NULL,
    granted_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (source_id, subject_id, permission)
);


-- Whether `p_subject` may `use` at least one source of image `p_image`: the
-- standard ownership ∨ grant disjunction, evaluated per source over
-- `principals()`. Because `principals()` folds the `everyone` subject in, a
-- source that grants `everyone` `use` (a public source) satisfies every subject.
CREATE FUNCTION tml_switchboard.image_source_usable (p_subject uuid, p_image uuid) returns boolean language sql stable AS $$
    select exists (
        select 1
        from tml_switchboard.image_sources s
        where s.image_id = p_image
          and (
              -- A principal of the subject is a global admin.
              exists (
                  select 1 from tml_switchboard.principals(p_subject) pr
                  -- The seeded `admins` group; see `ADMINS_GROUP_ID` in engine.rs.
                  where pr.id = '00000000-0000-0000-0000-000000000001'::uuid
              )
              -- A principal owns the source.
              or exists (
                  select 1 from tml_switchboard.principals(p_subject) pr
                  where pr.id = s.owner_subject
              )
              -- A principal holds a `use` grant on the source (`everyone` folded
              -- into principals(), so a public source matches every subject).
              or exists (
                  select 1
                  from tml_switchboard.image_source_grants g
                  join tml_switchboard.principals(p_subject) pr on g.subject_id = pr.id
                  where g.source_id = s.id and g.permission = 'use'
              )
          )
    );
$$;


-- A named, mutable image set.
--
-- `name` is the stable moving-target handle a job references (by id), such as
-- "ubuntu-2604".
--
-- Image membership lives in immutable per-generation snapshots
-- (`image_set_generations` / `image_set_members`). Sets are never deleted
-- (metadata is immortal), so a job that pinned a generation always resolves.
CREATE TABLE tml_switchboard.image_sets (
    id uuid NOT NULL PRIMARY KEY,
    name text NOT NULL UNIQUE,
    owner_subject uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    label text,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


-- One immutable snapshot of a set's membership. Every membership edit appends
-- a new generation (per-set monotonic from 1, allocated under an advisory
-- lock in the create-generation transaction). Append-only: a trigger blocks
-- UPDATE/DELETE (same pattern as audit_events). `created_by` is provenance.
CREATE TABLE tml_switchboard.image_set_generations (
    set_id uuid NOT NULL REFERENCES tml_switchboard.image_sets (id) ON DELETE CASCADE,
    generation int NOT NULL,
    created_by uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (set_id, generation),
    CONSTRAINT generation_positive CHECK (generation >= 1)
);


-- The members of ONE generation (full replacement per generation).
--
-- A member's eligibility is a set of required host tags: the member is
-- admissible for a host iff `hosts.tags` is a superset of `required_host_tags`.
--
-- Among admissible members the most specific (largest required set) wins, ties
-- broken by `index` (the member's explicit array position in the
-- create-generation request). Image selection uses HOST tags only; target/DUT
-- tags are irrelevant here. `image_id` references an immortal `images` row (no
-- cascade: images are never deleted).
CREATE TABLE tml_switchboard.image_set_members (
    set_id uuid NOT NULL,
    generation int NOT NULL,
    image_id uuid NOT NULL REFERENCES tml_switchboard.images (id),
    required_host_tags TEXT[] NOT NULL DEFAULT '{}',
    index int NOT NULL,
    PRIMARY KEY (set_id, generation, image_id),
    UNIQUE (set_id, generation, index),
    FOREIGN key (set_id, generation) REFERENCES tml_switchboard.image_set_generations (set_id, generation) ON DELETE CASCADE
);


-- Append-only enforcement for generations: a generation snapshot is immutable
-- once created (every membership edit appends a new generation). Functionally
-- identical to `deny_audit_event_change`, duplicated here so the IMAGE CATALOG
-- section does not forward-reference the AUDIT LOG section.
CREATE OR REPLACE FUNCTION tml_switchboard.deny_generation_change () returns trigger language plpgsql AS $$
begin
    raise exception 'image-set generations are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;


CREATE TRIGGER image_set_generations_append_only before
UPDATE
OR delete ON tml_switchboard.image_set_generations FOR each ROW
EXECUTE function tml_switchboard.deny_generation_change ();


-- Image-set permissions. Authorization is the standard ownership ∨ grant
-- disjunction evaluated via `principals()`; the owner holds `manage`
-- implicitly. All grants are user-managed (no irrevocable/fixed grants as jobs
-- have), so no revocability trigger.
CREATE TYPE tml_switchboard.image_set_permission AS enum('use', 'manage');


CREATE TABLE tml_switchboard.image_set_grants (
    set_id uuid NOT NULL REFERENCES tml_switchboard.image_sets (id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    permission tml_switchboard.image_set_permission NOT NULL,
    granted_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (set_id, subject_id, permission)
);


-- The jobs table's image references, added here because `jobs` is defined
-- before the IMAGE CATALOG section (forward-reference pattern, as for
-- `hosts.current_job`). Images and generations are immortal, so all of these
-- are ON DELETE NO ACTION.
ALTER TABLE tml_switchboard.jobs
ADD FOREIGN KEY (image_id) REFERENCES tml_switchboard.images (id) ON DELETE NO ACTION;


ALTER TABLE tml_switchboard.jobs
ADD FOREIGN KEY (resolved_image_id) REFERENCES tml_switchboard.images (id) ON DELETE NO ACTION;


ALTER TABLE tml_switchboard.jobs
ADD FOREIGN KEY (image_set_id, image_set_generation) REFERENCES tml_switchboard.image_set_generations (set_id, generation) ON DELETE NO ACTION;


-- ===========================================================================
-- AUDIT LOG
-- ===========================================================================
--
-- A curated, durable, append-only record of system state changes (and a few
-- observational events). Written in the SAME transaction as the change it
-- describes, via the `audit::transition`/`audit::emit` chokepoint in the Rust
-- code -- so a rolled-back state change never leaves an event behind, and an
-- event provably describes its mutation.
--
-- Payloads are stored as raw structured JSON and rendered at view time by a
-- per-event-type renderer keyed off `event_type`; the read API redacts based on
-- the viewer's permissions on the related entity. Secret VALUES (e.g. the
-- contents of `parameter_value.is_secret` parameters) must never be embedded in
-- a payload -- store the key name and a redacted marker instead. One row per
-- state change or observational event. Append-only: a trigger blocks
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
CREATE TABLE tml_switchboard.audit_events (
    event_id uuid NOT NULL PRIMARY KEY,
    event_type text NOT NULL,
    payload jsonb NOT NULL,
    actor_id uuid NOT NULL,
    correlation_id uuid,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


CREATE INDEX audit_events_created_at_idx ON tml_switchboard.audit_events (created_at);


-- The entity kinds an audit event can relate to. `subject` covers both users and
-- groups (matching the unified subjects model above); resource-scoped relations
-- use `job`, `host`, or `image_set`.
CREATE TYPE tml_switchboard.audit_entity_kind AS enum('job', 'host', 'subject', 'image_set');


-- An event's role with respect to a related entity. `actor` is the principal
-- that caused it; `subject` is the entity acted upon; `context` is any
-- additional entity whose viewers should also be able to see the event (e.g.
-- the host on which a job ran).
CREATE TYPE tml_switchboard.audit_role AS enum('actor', 'subject', 'context');


-- Links an event to an entity it relates to, with the view policy for THIS
-- relation. `entity_id` is a PLAIN uuid with NO foreign key: that is what keeps
-- audit history alive after the referenced job/host/subject is deleted
-- (matching the `owner_id on delete set null` provenance rationale above).
--
-- `view_policy` is the EXACT permission a viewer must hold on `entity` to see
-- the event through this relation -- one of the host/job permission enum values
-- ('read'|'start'|'ssh'|'stop'|'manage') stored as text so the same column
-- covers both resource kinds, plus the sentinel 'operator_only' for
-- system-internal events that never surface in a user-facing feed. Visibility
-- is disjunctive across relations: a viewer sees the event if they satisfy ANY
-- one relation's policy. Permission-implication (e.g. `manage` => `read`) is a
-- deliberate non-goal; matching is exact.
CREATE TABLE tml_switchboard.audit_event_relations (
    event_id uuid NOT NULL REFERENCES tml_switchboard.audit_events (event_id) ON DELETE CASCADE,
    entity_kind tml_switchboard.audit_entity_kind NOT NULL,
    entity_id uuid NOT NULL,
    role tml_switchboard.audit_role NOT NULL,
    view_policy text NOT NULL,
    PRIMARY KEY (event_id, entity_kind, entity_id, role)
);


-- The feed's hot path: "events for entity (kind, id), newest first". UUIDv7
-- `event_id` is time-ordered, so a DESC scan on this index also yields
-- chronological order without a separate sort.
CREATE INDEX audit_event_relations_entity_idx ON tml_switchboard.audit_event_relations (entity_kind, entity_id, event_id DESC);


-- Enforce append-only on `audit_events`: any direct UPDATE or DELETE raises.
-- Same trigger pattern as `deny_irrevocable_grant_change` above. FK-cascade
-- teardown of `audit_event_relations` when an event is deleted is not gated by
-- this trigger (it fires on `audit_events`, not relations), so a deliberate GC
-- that drops the trigger to remove whole events still cleans the relation rows
-- up via the cascade.
CREATE OR REPLACE FUNCTION tml_switchboard.deny_audit_event_change () returns trigger language plpgsql AS $$
begin
    raise exception 'audit events are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;


CREATE TRIGGER audit_events_append_only before
UPDATE
OR delete ON tml_switchboard.audit_events FOR each ROW
EXECUTE function tml_switchboard.deny_audit_event_change ();


-- =============================================================================
-- CHANGE NOTIFICATIONS
-- =============================================================================
--
-- Row-change wake-ups over LISTEN/NOTIFY, on the single channel `tml_events`.
-- Notifications are edge triggers, not data carriers: a receiver re-reads the
-- database instead of acting on the payload, so lost or duplicated
-- notifications are harmless (receivers keep their poll timers as a staleness
-- fallback). The payload carries only what routing needs:
--
--     {"table": <TG_TABLE_NAME>, "keys": {<column>: [<value>, ...], ...}}
--
-- The trigger arguments name the routing-key columns; the first must be the
-- table's primary key (never NULL, so an ordinary row event always has at
-- least one key). Each key maps to the distinct non-NULL old/new values, so an
-- UPDATE that moves a row between scopes wakes both. Because payloads are
-- minimal, Postgres' per-transaction dedup of identical notifications
-- coalesces repeated changes to the same row.
--
-- Bulk-write cap: past a per-table, per-transaction row count (defaulted in
-- the function body, overridable per session/transaction via the
-- `tml_switchboard.notify_row_limit` GUC), further rows emit the keyless form
-- `{"table": ...}` — identical
-- payloads, deduped to a single delivered notification — which receivers must
-- treat as "any row of this table may have changed".
CREATE OR REPLACE FUNCTION tml_switchboard.notify_change () returns trigger language plpgsql AS $$
declare
    guc text := 'tml_switchboard.notify_rows_' || TG_TABLE_NAME;
    n int := coalesce(nullif(current_setting(guc, true), ''), '0')::int + 1;
    row_limit int := coalesce(
        nullif(current_setting('tml_switchboard.notify_row_limit', true), ''),
        '100')::int;
    old_j jsonb;
    new_j jsonb;
    ks jsonb := '{}'::jsonb;
    col text;
    vals jsonb;
begin
    perform set_config(guc, n::text, true);
    if n > row_limit then
        perform pg_notify('tml_events',
            jsonb_build_object('table', TG_TABLE_NAME)::text);
        return null;
    end if;
    old_j := case when TG_OP <> 'INSERT' then to_jsonb(OLD) end;
    new_j := case when TG_OP <> 'DELETE' then to_jsonb(NEW) end;
    foreach col in array TG_ARGV loop
        select coalesce(jsonb_agg(distinct v), '[]'::jsonb) into vals
        from unnest(array[old_j -> col, new_j -> col]) as t (v)
        where v is not null and v <> 'null'::jsonb;
        if vals <> '[]'::jsonb then
            ks := ks || jsonb_build_object(col, vals);
        end if;
    end loop;
    perform pg_notify('tml_events',
        jsonb_build_object('table', TG_TABLE_NAME, 'keys', ks)::text);
    return null;
end;
$$;


-- UPDATE triggers are split from INSERT/DELETE so they can carry
-- `WHEN (OLD.* IS DISTINCT FROM NEW.*)`: an UPDATE that rewrites a row without
-- changing it emits nothing. This matters because reconciliation re-applies
-- the current state idempotently — without the guard, a consumer's own no-op
-- write would wake it again, sustaining a wake/write cycle at the debounce
-- rate.
CREATE TRIGGER jobs_notify_write
AFTER insert
OR delete ON tml_switchboard.jobs FOR each ROW
EXECUTE function tml_switchboard.notify_change ('job_id', 'dispatched_on_host_id');


CREATE TRIGGER jobs_notify_update
AFTER
UPDATE ON tml_switchboard.jobs FOR each ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)
EXECUTE function tml_switchboard.notify_change ('job_id', 'dispatched_on_host_id');


-- Column-filtered so that heartbeat refreshes (`last_seen_at`) never notify:
-- they arrive per host per ping interval and carry no scheduling edge. Host
-- connect still surfaces (a new worker increments `worker_instance_id`); a
-- clean disconnect only NULLs `last_seen_at` and deliberately stays silent —
-- no receiver acts on host death, the liveness cutoff covers it.
CREATE TRIGGER hosts_notify_write
AFTER insert
OR delete ON tml_switchboard.hosts FOR each ROW
EXECUTE function tml_switchboard.notify_change ('host_id');


CREATE TRIGGER hosts_notify_update
AFTER
UPDATE ON tml_switchboard.hosts FOR each ROW WHEN (
    -- Exclude `last_seen_at` from check
    (to_jsonb(OLD) - 'last_seen_at') IS DISTINCT FROM (to_jsonb(NEW) - 'last_seen_at')
)
EXECUTE FUNCTION tml_switchboard.notify_change ('host_id');
