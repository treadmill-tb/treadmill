drop schema if exists tml_switchboard cascade;

create schema tml_switchboard;

-- We want to be able to distinguish system activity from user activity, hence the differentiation between normal and
-- system users.
-- At the moment, there's not really a good picture of what a 'system' user might actually do, and that's reflected in
-- the structure of this table, as even though a system user rather obviously wouldn't have a password (or, likely, an
-- email), these fields are marked as 'non-null'.
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

-- API tokens are rather vitally important. They are in fact the only 'mark' of authorization: only with a token can a
-- request from a client be authorized.
-- API tokens can either have their own permissions, or inherit from the user that owns the token.
-- TODO: it is not clear whether an inheriting token can have its own privileges: however, it seems unlikely, since it
--       seems a logical invariant that privilege should be monotonic: a token's privileges should be a weakening of
--       those of the user that owns it.
-- API tokens have both an expiration system and a cancellation mechanism, which can be used to render a token invalid
-- and void before its natural expiration.
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

-- All supervisors must be registered with the switchboard database: any supervisor not registered will be turned away
-- at the gate, so to speak.
-- The name is at the moment superfluous and nowhere referenced, though that may change in the future.
-- The auth_token field is quite the opposite-a supervisor attempting to connect to the switchboard must provide proof
-- of its identity, and that is precisely what the auth_token provides: a 256-byte random string uniquely authenticating
-- the supervisor.
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
-- The one in the database exists for two purposes:
--  (1) as an archive for jobs that are no longer active and have passed from the memory of the ephemeral job table in
--      the switchboard
--  (2) as a persistent store which can be used to reconstruct the switchboard's state in the event of a restart
--
-- The jobs table contains all jobs that have been enqueued. Its purpose is to provide bare-bones information that can
-- be used to restore the switchboard's functional state. This means that it is not concerned with reporting of results
-- or the concrete state of the job, beyond the minimal that is required for state reconstruction.
create type tml_switchboard.restart_policy as
(
    remaining_restart_count integer
);
create type tml_switchboard.simple_state as enum (
    'queued',
    'running',
    'inactive'
    );
create table tml_switchboard.jobs
(
    job_id                   uuid                           not null primary key,

-- These specify the job image and the nature of the job:
--  (1) normal job: image_id is set, and both resume_job_id and restart_job_id are null
--  (2) restarted job: image_id is set, and restart_job_id is set
--  (3) resumed job: image_id is set, and resume_job_id is set
    resume_job_id            uuid references tml_switchboard.jobs (job_id) on delete no action,
    restart_job_id           uuid references tml_switchboard.jobs (job_id) on delete no action,
    image_id                 bytea,

-- SSH keys to be injected, if any
    ssh_keys                 text[]                         not null,
-- Restart policy
    restart_policy           tml_switchboard.restart_policy not null,
-- Token the job was enqueued by. Used to determine which supervisors a job can run on.
    enqueued_by_token_id     uuid                           not null references tml_switchboard.api_tokens (token_id) on delete no action,

-- Configuration that decides which supervisors it should run on
    tag_config               text                           not null,
-- The amount of time the job can run before it is killed.
    job_timeout              interval                       not null,

-- The time at which the job was queued. If after [config:api.jobs.queue_timeout] the job is still queued, it will be
-- canceled.
    queued_at                timestamp with time zone       not null,
-- These specify the latest known state of the job, which is used to determine how the switchboard should proceed in the
-- event that a network partition occurs, or in general when a component of the system fails in such a way as to
-- invalidate or otherwise clear the ephemeral state in the supervisor.
    simple_state             tml_switchboard.simple_state   not null,
-- The time at which the job was started.
    started_at               timestamp with time zone,
-- It is necessary to record the ID of the supervisor on which the job is running. If both switchboard and supervisor
-- fail, then upon supervisor reconnection, the switchboard must be able to discern the failure of the job; otherwise,
-- the only recourse of the user is to wait for the job to time out.
    running_on_supervisor_id uuid,

-- There are two allowed states:
-- resume_job_id = null, image_id != null, restart_job_id = _
-- resume_job_id != null, image_id = null, restart_job_id = null
    check
        ((resume_job_id is null and (image_id is not null))
        or (resume_job_id is not null and restart_job_id is null and image_id is null)),
-- ImageId is 32 bytes
    check
        (case
             when image_id is not null then octet_length(image_id) = 32
             else true
        end),
-- queued => started_at, R.O.S.I = null
    check (simple_state != 'queued' or (started_at is null and running_on_supervisor_id is null)),
-- running => started_at, R.O.S.I != null
    check (simple_state != 'running' or (started_at is not null and running_on_supervisor_id is not null))
);

create type tml_switchboard.parameter_value as
(
    value     text,
    is_secret bool
);
create table tml_switchboard.job_parameters
(
    job_id uuid                            not null references tml_switchboard.jobs (job_id) on delete cascade,
    key    text                            not null,
    value  tml_switchboard.parameter_value not null,

    primary key (job_id, key)
);

create type tml_switchboard.exit_status as enum (
    -- no supervisors in "supervisors" that can run this that you have permissions for
    'failed_to_match',
    -- too long in queue
    'queue_timeout',
    -- failed to launch this on a supervisor (may incur a retry)
    'host_start_failure', -- e.g., supervisor timeout, image failed to fetch,
    -- host reported an error (i.e., through puppet)
    'host_terminated_with_error',
    -- host reported success
    'host_terminated_with_success',
    -- job timeout
    'host_terminated_timeout',
    -- job was stopped by user
    'job_canceled',
    -- supervisor was unregistered while job was running
    'unregistered_supervisor',
    -- host disappeared while running, and when it came back, the job was gone
    'host_dropped_job'
    );
create table tml_switchboard.job_results
(
    job_id        uuid                        not null primary key references tml_switchboard.jobs (job_id) on delete cascade,
    supervisor_id uuid references tml_switchboard.supervisors (supervisor_id) on delete no action,

    exit_status   tml_switchboard.exit_status not null,
    host_output   jsonb,

    terminated_at timestamp with time zone    not null,

    check (case
               when exit_status = 'failed_to_match' then host_output is null
               when exit_status = 'queue_timeout' then host_output is null
               when exit_status = 'host_terminated_timeout' then host_output is null
               when exit_status = 'job_canceled' then host_output is null
               when exit_status = 'host_dropped_job' then host_output is null
               else host_output is not null
        end)
);

create table tml_switchboard.job_state_history
(
    job_id    uuid                     not null references tml_switchboard.jobs (job_id) on delete cascade,
    job_state jsonb                    not null,

    logged_at timestamp with time zone not null
);
