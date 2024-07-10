--
-- Switchboard database schema.
--

-- CORE STRUCTURE ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

drop table if exists job_results;
drop table if exists jobs;
drop type if exists job_parameter;

drop table if exists user_sessions;
drop table if exists user_presessions;
drop type if exists fingerprint;

drop table if exists maintenance_schedule;

drop table if exists supervisors;

drop table if exists api_token_privileges;
drop table if exists user_privileges;

drop table if exists api_tokens;
drop type if exists cancellation;

drop table if exists users;

-- BASE ENTITIES

create table users
(
    user_id       uuid                     not null primary key,
    name          text                     not null unique,
    email         text                     not null unique,
    password_hash text                     not null,

    created_at    timestamp with time zone not null,
    last_login_at timestamp with time zone not null,
    locked        bool                     not null
);

create type cancellation as
(
    canceled_at         timestamp with time zone,
    cancellation_reason text
);
create table api_tokens
(
-- internal identifier of the token
    token_id           uuid                     not null primary key,
-- actual contents of the token; random nonce
    token              bytea                    not null unique,

-- the user who created this token
    created_by_user_id uuid                     not null references users (user_id) on delete cascade,

-- whether the token was revoked/invalidated/etc. or not
    canceled           cancellation,

-- expiration date
    created_at         timestamp with time zone not null,
    expires_at         timestamp with time zone not null,

-- resource limits
    limit_policy       bytea                    not null,
    limit_quotas       bytea                    not null
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
    supervisor_id           uuid primary key         not null,
    name                    text                     not null,

    created_at              timestamp with time zone not null,
    last_connected_at       timestamp with time zone not null,
    created_by_admin_id     uuid                     not null references users on delete no action,

    public_key              text                     not null,

    last_reported_status    bytea                    not null,
    last_reported_status_at timestamp with time zone not null,
    tags                    text[]                   not null
);

-- MAINTENANCE ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

-- Scheduled system maintenance intervals; so that the system can schedule around updates, etc.
create table maintenance_schedule
(
    begins_at          timestamp with time zone not null,
    ends_at            timestamp with time zone not null,
    reason             text                     not null,
    created_by_user_id uuid                     not null references users
);

-- WEB SECURITY ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

-- CSRF prevention / mitigation

create table user_presessions
(
    presession_id uuid                     not null primary key,
    csrf_token    bytea                    not null,
    user_agent    text                     not null,
    remote_ip     inet                     not null,
    created_at    timestamp with time zone not null,
    expires_at    timestamp with time zone not null
);
create table user_sessions
(
    session_id uuid                     not null primary key,
    csrf_token bytea                    not null,
    user_agent text                     not null,
    remote_ip  inet                     not null,
    created_at timestamp with time zone not null,
    expires_at timestamp with time zone not null,

    user_id    uuid                     not null references users
);

-- BUSINESS LOGIC ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

create type job_parameter as
(
    k      text,
    v      text,
    secret bool
);
create table jobs
(
    request_id           uuid             not null,
    job_id               uuid primary key not null,
    resume_job           bool             not null,
    image_id             uuid             not null,
    ssh_keys             text[]           not null,

    parameters           job_parameter[]  not null,
    run_command          text,

    enqueued_by_token_id uuid             not null references api_tokens (token_id) on delete no action,

    tag_config           text             not null
);
create table job_results
(
    job_id            uuid                     not null references jobs (job_id),
    supervisor_id     uuid                     not null references supervisors (supervisor_id) on delete no action,
    run_number        integer                  not null,

    status            text                     not null,
    result            text,
    output            bytea,

    enqueued_at       timestamp with time zone not null,
    started_setup_at  timestamp with time zone not null,
    started_runner_at timestamp with time zone not null,
    terminated_at     timestamp with time zone not null
);
