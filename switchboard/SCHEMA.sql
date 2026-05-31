-- -*- fill-column: 80; -*-

create schema tml_switchboard;

-- We want to be able to distinguish system activity from user activity, hence
-- the differentiation between normal and system users.
--
-- At the moment, there's not really a good picture of what a 'system' user
-- might actually do, and that's reflected in the structure of this table, as
-- even though a system user rather obviously wouldn't have a password (or,
-- likely, an email), these fields are marked as 'non-null'.
--
-- TODO: discussion
create type tml_switchboard.user_type as enum ('normal', 'system');
create table tml_switchboard.users
(
    user_id       uuid                      not null primary key,
    name          text                      not null unique,
    email         text                      not null unique,
    password_hash text                      not null,

    user_type     tml_switchboard.user_type not null,
    locked        bool                      not null default false
);

-- API tokens are rather vitally important. They are in fact the only 'mark' of
-- authorization: only with a token can a request from a client be authorized.
--
-- API tokens can either have their own permissions, or inherit from the user
-- that owns the token.
--
-- TODO: it is not clear whether an inheriting token can have its own
--       privileges: however, it seems unlikely, since it seems a logical
--       invariant that privilege should be monotonic: a token's privileges
--       should be a weakening of those of the user that owns it.
--
-- API tokens have both an expiration system and a cancellation mechanism, which
-- can be used to render a token invalid and void before its natural expiration.
create type tml_switchboard.api_token_cancellation as
(
    canceled_at         timestamp with time zone,
    cancellation_reason text
);
create table tml_switchboard.api_tokens
(
    token_id                  uuid                     not null primary key,
    token                     bytea                    not null unique,
    user_id                   uuid                     not null references tml_switchboard.users (user_id) on delete cascade,
    inherits_user_permissions bool                     not null,
    canceled                  tml_switchboard.api_token_cancellation,
    created_at                timestamp with time zone not null,
    expires_at                timestamp with time zone not null,

    check (octet_length(token) = 128)
);

-- Privileges are assignments of permissions to subjects (users and tokens). Note that is not necessary for a token to
-- explicitly be assigned privileges (the inheritance clause described for API tokens above).
create table tml_switchboard.user_privileges
(
    user_id    uuid not null references tml_switchboard.users (user_id) on delete cascade,
    permission text not null,

    primary key (user_id, permission)
);
create table tml_switchboard.api_token_privileges
(
    token_id   uuid not null references tml_switchboard.api_tokens (token_id) on delete cascade,
    permission text not null,

    primary key (token_id, permission)
);

-- Host + Port tuple for ssh_endpoints of a supervisor.
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
-- registered will be turned away at the gate, so to speak.
--
-- The name is at the moment superfluous and nowhere referenced, though that may
-- change in the future.  The auth_token field is quite the opposite-a
-- supervisor attempting to connect to the switchboard must provide proof of its
-- identity, and that is precisely what the auth_token provides: a 256-byte
-- random string uniquely authenticating the supervisor.
--
-- TODO: The tags remain ill-specified.
create table tml_switchboard.supervisors
(
    supervisor_id      uuid   not null primary key,
    name               text   not null,
    auth_token         bytea  not null unique,
    tags               text[] not null,

    -- SSH endpoints that all jobs executing on this host are reachable
    -- under. Currently this mapping is static, in the future we may change it
    -- to use a different endpoint per job, even if running on the same
    -- host. Thus, each job also contains its own endpoint table, populated from
    -- this set of host endpoints.
    --
    -- Each endpoint is a "hostname:port" tuple. IPv6 addreses are to be
    -- enclosed in square brackets, such as "[::1]:22".
    ssh_endpoints      tml_switchboard.ssh_endpoint[] not null,

    -- A supervisor can either be idle or have a job assigned. This column keeps
    -- track of this state. If a job is assigned, then the supervisor is not
    -- idle, and the "sub-state" is determined through the job's state data.
    --
    -- This column is irrespective of whether the supervisor is currently
    -- connected to the switchboard. Even if it is disconnected, this field
    -- tracks the last-known supervisor state. It will be reconciled and updated
    -- when the supervisor next reconnects.
    --
    -- This is the *canonical* "assigned job" pointer. The invariant
    --
    --     supervisors.current_job = J  =>
    --       jobs[J].dispatched_on_supervisor_id = this supervisor
    --
    -- is asserted by the worker in its `with_txn` transitions (the worker is the
    -- sole writer); a DB trigger would be awkward under Atlas. The partial unique
    -- index below enforces the half of the invariant that *is* expressible as a
    -- pure constraint: a job is `current_job` for at most one supervisor.
    --
    -- Foreign key constrained added after `jobs` is defined below.
    current_job        uuid,

    -- Each connected supervisor is serviced by a worker. To prevent multiple
    -- concurrent workers claiming ownership of the same supervisor, we use the
    -- database to assign each supervisor--worker combination a unique "worker
    -- instance ID". This monotonically increasing value is obtained when a
    -- supervisor connects, and checked against by any subsequent database
    -- operation performed by that worker. If it changes, the worker assumes
    -- that a newer instance exists and terminates.
    worker_instance_id bigint NOT NULL DEFAULT 0,

    check (octet_length(auth_token) = 128),
    check (worker_instance_id >= 0)
);

-- The job table is the beating heart of the switchboard.
--
-- The one in the database exists for three purposes:
--
--  (1) as an archive for jobs that are no longer active and have passed from
--      the memory of the ephemeral job table in the switchboard
--
--  (2) as a persistent store which can be used to reconstruct the switchboard's
--      state in the event of a restart
--
--  (3) as a compendium of the results of jobs that have finished
--
-- The jobs table contains all jobs that have been enqueued. Its purpose is to
-- provide bare-bones information that can be used to restore the switchboard's
-- functional state, and record the final exit status of all finalized jobs. It
-- does not aim to record the concrete state of the job, beyond the minimum that
-- is required for state reconstruction.

-- Restart policy dictates the number of times a job can be restarted
-- automatically by the switchboard.
create type tml_switchboard.restart_policy as
(
    remaining_restart_count integer
);

-- The job's execution lifecycle state. This is the materialized current state
-- of the job, tracking where in its life it is. It mirrors the wire-level
-- `RunningJobState` for the assigned sub-states, with switchboard-owned
-- bookends (`queued`, `scheduled`, `finalized`).
create type tml_switchboard.job_state as enum (
    -- Accepted, no supervisor assigned yet; eligible for scheduling.
    'queued',
    -- Bound to a supervisor but StartJob not yet sent; no longer eligible for
    -- (re)scheduling. dispatched_on_supervisor_id set, started_at null.
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

-- Why a job terminated. Set once, at finalization. This is orthogonal to the
-- task's exit status (success/failure of the user workload) and to any exit
-- message.
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
    'supervisor_match_error',
    'supervisor_host_start_failure',
    'supervisor_dropped_job',
    'supervisor_unreachable',
    'resume_failed',
    'internal_error'
    );

-- The supervisor-reported outcome of the user's workload (its "task outcome").
-- Set out-of-band of termination and revisable while the job is assigned:
-- `pending` until the result is known, then `success` or `failure`. Once set it
-- is never cleared. Orthogonal to termination_reason: may be present (or absent)
-- regardless of why the job terminated.
create type tml_switchboard.task_exit_status as enum (
    'pending',
    'success',
    'failure'
    );

create table tml_switchboard.jobs
(
    job_id                      uuid                             not null primary key,

    -- These specify the job image and the nature of the job:
    --  (1) normal job: image_id is set, and both resume_job_id and
    --      restart_job_id are null
    --  (2) restarted job: image_id is set, and restart_job_id is set
    --  (3) resumed job: image_id is set, and resume_job_id is set
    resume_job_id               uuid references tml_switchboard.jobs (job_id) on delete no action,
    restart_job_id              uuid references tml_switchboard.jobs (job_id) on delete no action,
    image_id                    bytea,

    -- SSH keys to be injected, if any
    ssh_keys                    text[]                           not null,

    -- Restart policy
    restart_policy              tml_switchboard.restart_policy   not null,

    -- Token the job was enqueued by. Used to determine which supervisors a job
    -- can run on.
    enqueued_by_token_id        uuid                             not null references tml_switchboard.api_tokens (token_id) on delete no action,

    -- Configuration that decides which supervisors it should run on
    tag_config                  text                             not null,

    -- The amount of time the job can run before it is killed.
    job_timeout                 interval                         not null,

    -- The latest known execution-lifecycle state of the job, used to determine
    -- how the switchboard should proceed in the event that a network partition
    -- occurs, or in general when a component of the system fails in such a way
    -- as to invalidate or otherwise clear the ephemeral state in the supervisor.
    job_state                   tml_switchboard.job_state        not null,

    -- The sub-stage while `job_state = 'initializing'`; null otherwise.
    initializing_stage          tml_switchboard.job_initializing_stage,

    -- The time at which the job was queued. If after
    -- [config:api.jobs.queue_timeout] the job is still queued, it will be
    -- canceled.
    queued_at                   timestamp with time zone         not null,

    -- The time at which the job was started.
    started_at                  timestamp with time zone,

    -- It is necessary to record the ID of the supervisor to which the job has
    -- been dispatched. If both switchboard and supervisor fail, then upon
    -- supervisor reconnection, the switchboard must be able to discern the
    -- failure of the job; otherwise, the only recourse of the user is to wait
    -- for the job to time out.
    dispatched_on_supervisor_id uuid,

    -- SSH endpoints that this job is reachable under.
    --
    -- This field will be populated once this job is scheduled on a
    -- host, if that host exposes any SSH endpoints.
    --
    -- Each endpoint is a "hostname:port" tuple. IPv6 addreses are to be
    -- enclosed in square brackets, such as "[::1]:22".
    ssh_endpoints               tml_switchboard.ssh_endpoint[],

    -- Filled out when transitioned into `finalized` job state.
    --
    -- `termination_reason` records *why* the job stopped; `task_exit_status`
    -- records the *result of the user workload* (if it ran and reported one),
    -- and is independent of the reason; `exit_message` is an optional
    -- human-readable note (multi-line markdown acceptable) for any reason.
    -- Captured workload output is intentionally not stored here (it belongs in
    -- object storage).
    termination_reason          tml_switchboard.termination_reason,
    task_exit_status            tml_switchboard.task_exit_status,
    exit_message                text,
    terminated_at               timestamp with time zone,

    -- For bookkeeping purposes; for ease of use, for now this works by just
    -- putting `last_updated_at = DEFAULT` on UPDATE queries, as per
    -- https://www.morling.dev/blog/last-updated-columns-with-postgres/. The
    -- original intention was to have something like an ON UPDATE trigger, but
    -- since it seems a bit messy to do with Postgres, it's being left alone for
    -- now.
    last_updated_at             timestamp with time zone         not null default current_timestamp,

    ---->> INVARIANT CHECKING <<----

    -- There are two allowed states:
    --  (1) resume_job_id = null, image_id != null, restart_job_id = _
    --  (2) resume_job_id != null, image_id = null, restart_job_id = null
    constraint valid_init_spec check (
      (resume_job_id is null and (image_id is not null))
        or (resume_job_id is not null and restart_job_id is null and image_id is null)
    ),

    -- ImageId is 32 bytes
    constraint valid_image_id check (
      case
        when image_id is not null then octet_length(image_id) = 32
        else true
      end
    ),

    -- Restart count >= 0
    constraint valid_restart_policy check (
      (restart_policy).remaining_restart_count >= 0
    ),

    -- A supervisor is bound from `scheduled` onwards (and stays recorded through
    -- `finalized`); `dispatched_on_supervisor_id` tracks that binding for the
    -- assigned, not-yet-terminal lifecycle states.
    constraint dispatched_supervisor_iso_assigned check (
      (dispatched_on_supervisor_id is not null) =
        (job_state in ('scheduled', 'initializing', 'ready', 'terminating'))
    ),

    -- `started_at` is set exactly while the supervisor is executing the job, i.e.
    -- once dispatched (`initializing`) and until terminal.
    constraint started_at_iso_executing check (
      (started_at is not null) =
        (job_state in ('initializing', 'ready', 'terminating'))
    ),

    -- The terminal record (`termination_reason`, `terminated_at`) is set exactly
    -- when the job is `finalized`.
    constraint termination_reason_iso_finalized check (
      (termination_reason is not null) = (job_state = 'finalized')
    ),
    constraint terminated_at_iso_finalized check (
      (terminated_at is not null) = (job_state = 'finalized')
    ),

    -- `task_exit_status` is the supervisor-reported task outcome, set out-of-band
    -- of termination once the job is dispatched. It is absent while `queued` (no
    -- supervisor is assigned yet) and may be present in any later state.
    constraint task_exit_status_absent_while_queued check (
      task_exit_status is null or job_state <> 'queued'
    ),

    -- `initializing_stage` is present exactly while `initializing`.
    constraint initializing_stage_iso_initializing check (
      (initializing_stage is not null) = (job_state = 'initializing')
    )
);

ALTER TABLE tml_switchboard.supervisors
    ADD FOREIGN KEY (current_job)
    REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION;

-- A job may be the `current_job` of at most one supervisor. Partial (only over
-- non-null `current_job`) so idle supervisors don't collide on NULL.
CREATE UNIQUE INDEX supervisors_current_job_unique
    ON tml_switchboard.supervisors (current_job)
    WHERE current_job IS NOT NULL;

create type tml_switchboard.parameter_value as
(
    value     text,
    is_secret bool
);

-- This is a table of its own because per-job parameter key uniqueness is a
-- useful property to enforce.
create table tml_switchboard.job_parameters
(
    job_id uuid                            not null references tml_switchboard.jobs (job_id) on delete cascade,
    key    text                            not null,
    value  tml_switchboard.parameter_value not null,

    primary key (job_id, key)
);
