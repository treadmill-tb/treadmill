--
-- Switchboard database schema.
--

-- CORE STRUCTURE ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

drop table if exists job_results;
drop type if exists exit_status;
drop table if exists job_parameters;
drop type if exists parameter_value;
drop table if exists jobs;
drop table if exists images;
drop type if exists restart_policy;

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

    public_key        text                     not null,

    tags              text[]                   not null
);

-- BUSINESS LOGIC ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

create type restart_policy as
(
    remaining_restart_count integer
);

create table images
(
    image_id bytea not null primary key
);

create table jobs
(
    job_id               uuid           not null primary key,
    resume_job_id        uuid references jobs (job_id) on delete no action,
    restart_job_id       uuid references jobs (job_id) on delete no action,
-- MC: for now, since image management isn't really worked out, just completely don't bother with this
--      reason: having the foreign key constraint makes testing fixtures more complicated
    image_id             bytea, --references images (image_id) on delete no action,

    ssh_keys             text[]         not null,

    -- run_command          text,
    restart_policy       restart_policy not null,

    queued               bool           not null default true,

    enqueued_by_token_id uuid           not null references api_tokens (token_id) on delete no action,

    tag_config           text           not null,
    check
        ((resume_job_id::int + restart_job_id::int + image_id::int) = 1)
);

create type parameter_value as
(
    value  text,
    secret bool
);
create table job_parameters
(
    job_id uuid            not null references jobs (job_id) on delete cascade,
    key    text            not null,
    value  parameter_value not null,

    primary key (job_id, key)
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
    'host_terminated_timeout'
    );

create table job_results
(
    job_id        uuid                     not null references jobs (job_id),
    supervisor_id uuid                     not null references supervisors (supervisor_id) on delete no action,

    exit_status   exit_status              not null,
    host_output   jsonb,

    enqueued_at   timestamp with time zone not null,
    started_at    timestamp with time zone not null,
    terminated_at timestamp with time zone not null
);
