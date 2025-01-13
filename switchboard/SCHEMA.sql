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
    supervisor_id uuid   not null primary key,
    name          text   not null,
    auth_token    bytea  not null unique,
    tags          text[] not null,

    check (octet_length(auth_token) = 128)
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

-- Functional state is an internal property of jobs in the switchboard that
-- describes how the job is treated by the switchboard; it can be treated as a
-- simplification of execution status that only concerns itself with the
-- requirements of the switchboard.
create type tml_switchboard.functional_state as enum (
    -- The job is waiting to be dispatched to a supervisor
    'queued',
    -- The job has been dispatched to a supervisor
    'dispatched',
    -- The job is no longer active, and the exit status has been finalized.
    'finalized'
    );

-- The exit status of the job. See the job lifecycle documentation in the
-- internals section of the Treadmill book.
create type tml_switchboard.exit_status as enum (
    -- INTERNAL
    'supervisor_match_error',
    'internal_supervisor_error',
    'supervisor_host_start_error',
    'supervisor_dropped_job',
    -- TIMEOUT
    'queue_timeout',
    'job_timeout',
    -- EXTERNAL
    'job_canceled',
    'workload_finished_success',
    'workload_finished_error',
    'workload_finished_unknown'
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

    -- These specify the latest known state of the job, which is used to
    -- determine how the switchboard should proceed in the event that a network
    -- partition occurs, or in general when a component of the system fails in
    -- such a way as to invalidate or otherwise clear the ephemeral state in the
    -- supervisor.
    functional_state            tml_switchboard.functional_state not null,

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

    exit_status                 tml_switchboard.exit_status,
    -- Optional host output
    host_output                 text,
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

    -- queued => started_at, R.O.S.I = null
    -- running => started_at, R.O.S.I != null
    constraint valid_queued_implication check (
      functional_state != 'queued' or
        (started_at is null and dispatched_on_supervisor_id is null)
    ),
    constraint valid_dispatched_implication check (
      functional_state != 'dispatched' or
        (started_at is not null and dispatched_on_supervisor_id is not null)
    ),

    -- Restart count >= 0
    constraint valid_restart_policy check (
      (restart_policy).remaining_restart_count >= 0
    ),

    -- Exit status vs. host output
    constraint exit_status_allows_host_output check (
      case
        when exit_status = 'supervisor_match_error' then host_output is null
        when exit_status = 'queue_timeout' then host_output is null
        when exit_status = 'job_timeout' then host_output is null
        when exit_status = 'job_canceled' then host_output is null
        when exit_status = 'supervisor_dropped_job' then host_output is null
        when exit_status is null then host_output is null
        else true
      end
    ),

    -- exit status != null <=> functional state == finalized
    constraint exit_status_nullity_iso_finalized check (
      (exit_status is not null) = (functional_state = 'finalized')
    )
);

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

-- The histories of state transitions & exit statuses for the jobs
create table tml_switchboard.job_events
(
    job_id    uuid                     not null references tml_switchboard.jobs (job_id) on delete cascade,
    job_event jsonb                    not null,

    logged_at timestamp with time zone not null default current_timestamp,

    check (
      job_event ? 'event_type'
        and (
          job_event ->> 'event_type' = 'state_transition'
            or job_event ->> 'event_type' = 'declare_workload_exit_status'
            or job_event ->> 'event_type' = 'set_exit_status'
            or job_event ->> 'event_type' = 'finalize_result'
        )
    )
);
