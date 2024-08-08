--
-- Switchboard database schema.
--

-- CORE STRUCTURE ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

drop table if exists job_results;
drop type if exists exit_status;
drop table if exists job_parameters;
drop type if exists parameter_value;
drop table if exists jobs;
drop type if exists job_known_state;
drop type if exists rendezvous_server_spec;
drop type if exists restart_policy;
drop table if exists images;

drop table if exists supervisors;

drop table if exists api_token_privileges;
drop table if exists user_privileges;

drop table if exists api_tokens;
drop type if exists api_token_cancellation;

drop table if exists users;
drop type if exists user_type;

drop table if exists events;

-- EVENT LOGGING

create table events
(
    event_id uuid  not null primary key,
    data     jsonb not null
);

-- BASE ENTITIES

create type user_type as enum ('normal', 'system');
create table users
(
    user_id       uuid                     not null primary key,
    name          text                     not null unique,
    email         text                     not null unique,
    password_hash text                     not null,

    user_type     user_type                not null,

    created_at    timestamp with time zone not null,
    last_login_at timestamp with time zone not null,
    locked        bool                     not null
);

create type api_token_cancellation as
(
    canceled_at         timestamp with time zone,
    cancellation_reason text
);
create table api_tokens
(
-- internal identifier of the token
    token_id                  uuid                     not null primary key,
-- actual contents of the token; random nonce
    token                     bytea                    not null unique,

-- the user who created this token
    user_id                   uuid                     not null references users on delete cascade,
    inherits_user_permissions bool                     not null,

-- whether the token was revoked/invalidated/etc. or not
    canceled                  api_token_cancellation,

-- expiration date
    created_at                timestamp with time zone not null,
    expires_at                timestamp with time zone not null
);

-- PERMISSIONS

create table user_privileges
(
    user_id    uuid not null references users on delete cascade,
    permission text not null,

    unique (user_id, permission)
);
create table api_token_privileges
(
    token_id   uuid not null references api_tokens on delete cascade,
    permission text not null,

    unique (token_id, permission)
);

-- SUPERVISORS

create table supervisors
(
    supervisor_id     uuid                     not null primary key,
    name              text                     not null,

    last_connected_at timestamp with time zone not null,

    auth_token        bytea                    not null unique,

    tags              text[]                   not null
);

-- BUSINESS LOGIC ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

create table images
(
    image_id bytea not null primary key
);

create type restart_policy as
(
    remaining_restart_count integer
);
create type rendezvous_server_spec as
(
    client_id       uuid,
    server_base_url text,
    auth_token      text
);
-- Used to store information about the general state of a job so that in the case of disconnection between supervisor
-- and switchboard, the switchboard is able to make informed decisions about whether it should try to restart the job or
-- not.
create type job_known_state as enum (
    -- The job has not run yet
    'queued',
    -- The job is currently running. In a precise sense, the real meaning is "the job MAY be running", however, the
    -- switchboard should treat "may be running" as "is running"; thus, this state is called 'running'.
    'running',
    -- The job is not running and does not need to run.
    'not_queued');
create table jobs
(
    job_id                 uuid                     not null primary key,

    resume_job_id          uuid references jobs (job_id) on delete no action,
    restart_job_id         uuid references jobs (job_id) on delete no action,
    image_id               bytea,

    ssh_keys               text[]                   not null,
    ssh_rendezvous_servers rendezvous_server_spec[] not null,

    restart_policy         restart_policy           not null,
    enqueued_by_token_id   uuid                     not null references api_tokens (token_id) on delete no action,

    tag_config             text                     not null,

    known_state            job_known_state          not null,
    timeout                interval                 not null,

    queued_at              timestamp with time zone not null,
    started_at             timestamp with time zone,

    check
        (((resume_job_id is not null)::int
        + (restart_job_id is not null)::int
        + (image_id is not null)::int)
        = 1
        )
);

create type parameter_value as
(
    value  text,
    secret bool
);
create table job_parameters
(
    job_id UUID NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    secret BOOLEAN NOT NULL,
    PRIMARY KEY (job_id, key)
);

create type exit_status as enum (
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
    'job_canceled'
    );

create table job_results
(
    job_id        uuid                     not null references jobs (job_id),
    -- will be null if job is not attached to supervisor
    supervisor_id uuid references supervisors (supervisor_id) on delete no action,

    exit_status   exit_status              not null,
    host_output   jsonb,

    terminated_at timestamp with time zone not null
);
