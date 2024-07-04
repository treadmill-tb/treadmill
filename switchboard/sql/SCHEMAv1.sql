-- CORE STRUCTURE ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

drop table if exists job_results;
drop table if exists jobs;
drop type if exists job_parameter;

drop table if exists user_sessions;
drop table if exists user_pre_sessions;
drop type if exists fingerprint;

drop table if exists maintenance_schedule;

drop table if exists vhd_perm_edit_tags;
drop table if exists vhd_perm_view_status;
drop table if exists vhd_perm_discover;
drop table if exists vhd_perm_session;
drop table if exists vhd_perm_queue;
drop table if exists vhd_feature_tags;
drop table if exists vhds;
drop table if exists supervisors;

drop table if exists api_token_groups;
drop table if exists api_tokens;
drop type if exists cancellation;

drop table if exists feature_tags;

drop table if exists group_users;
drop table if exists group_supervisors;
drop table if exists groups;

drop table if exists tml_admins;
drop table if exists users;
drop table if exists superusers;

-- BASE ENTITIES

create table superusers
(
    supervisor_id uuid primary key            not null,
    name          text                        not null,
    email         text                        not null,
    password_hash text                        not null,

    created_at    timestamp without time zone not null,
    last_login_at timestamp without time zone not null,
    locked        bool                        not null
);
create table users
(
    user_id       uuid primary key            not null,
    name          text                        not null,
    email         text                        not null,
    password_hash text                        not null,

    created_at    timestamp without time zone not null,
    last_login_at timestamp without time zone not null,
    locked        bool                        not null
);
create table tml_admins
(
    admin_id      uuid primary key            not null,
    name          text                        not null,
    email         text                        not null,
    password_hash text                        not null,

    created_at    timestamp without time zone not null,
    last_login_at timestamp without time zone not null
);

-- GROUPS

create table groups
(
    group_id uuid primary key not null,
    name     text             not null
);
create table group_supervisors
(
    group_id      uuid                        not null references groups on delete cascade,
    supervisor_id uuid                        not null references superusers on delete cascade,

    appointed_at  timestamp without time zone not null
);
create table group_users
(
    group_id               uuid                        not null references groups on delete cascade,
    user_id                uuid                        not null references users on delete cascade,

    appointed_at           timestamp without time zone not null,
    added_by_supervisor_id uuid                        not null references superusers (supervisor_id) on delete no action
);

-- FEATURE TAGS

create table feature_tags
(
    feature_tag_id                 uuid primary key            not null,
    owning_group_id                uuid                        not null references groups (group_id) on delete cascade,

    name                           text                        not null,

-- Metadata
    created_by_supervisor_id       uuid references superusers (supervisor_id) on delete no action,
    created_at                     timestamp without time zone not null,
    last_modified_by_supervisor_id uuid references superusers (supervisor_id) on delete no action,
    last_modified_at               timestamp without time zone not null
);

-- API TOKENS

create type cancellation as
(
    cancelled_at        timestamp without time zone,
    cancellation_reason text
);
-- API tokens
create table api_tokens
(
-- internal identifier of the token
    token_id           uuid primary key            not null,
-- actual contents of the token; random nonce
-- TODO: any kind of signing?
    token              bytea                       not null,

-- the user who created this token
    created_by_user_id uuid                        not null references users (user_id) on delete cascade,

-- whether the token was revoked/invalidated/etc. or not
    canceled           cancellation default null,

-- expiration date
    created_at         timestamp without time zone not null,
    expires_at         timestamp without time zone not null,

-- resource limits
    limit_policy       bytea                       not null,
    limit_quotas       bytea                       not null
);
-- Association of permission groups to API tokens.
create table api_token_groups
(
    token_id uuid not null references api_tokens on delete cascade,
    group_id uuid not null references groups on delete cascade
);

-- SUPERVISORS AND VHDS

-- Set of all known supervisor systems
create table supervisors
(
-- General information
    supervisor_id       uuid primary key            not null,
    name                text                        not null,

-- The group whose superusers have management rights over this supervisor
    owning_group_id     uuid                        not null references groups (group_id) on delete restrict,

-- Metadata
    created_at          timestamp without time zone not null,
    last_connected_at   timestamp without time zone not null,
    created_by_admin_id uuid                        not null references tml_admins (admin_id) on delete no action,

-- Public key of the supervisor
    public_key          bytea                       not null
);
-- Virtual Host-Device pairs
create table vhds
(
    vhd_id            bigserial primary key       not null,
    vhd_supervisor_id uuid                        not null references supervisors (supervisor_id) on delete cascade,

    added_at          timestamp without time zone not null,

    status            bytea                       not null
);
-- the set of feature tags associated with this VHD
create table vhd_feature_tags
(
    vhd_id         bigint not null references vhds on delete cascade,
    feature_tag_id uuid   not null references feature_tags on delete cascade
);
-- whether tokens attached to a certain permission group can enqueue jobs on this VHD
create table vhd_perm_queue
(
    vhd_id   bigint not null references vhds on delete cascade,
    group_id uuid   not null references groups on delete cascade
);
-- whether tokens attached to a certain permission group can open remote sessions on this VHD
create table vhd_perm_session
(
    vhd_id   bigint not null references vhds on delete cascade,
    group_id uuid   not null references groups on delete cascade
);
-- whether entities attached to a certain permission group can discover this VHD
create table vhd_perm_discover
(
    vhd_id   bigint not null references vhds on delete cascade,
    group_id uuid   not null references groups on delete cascade
);
-- whether entities attached to a certain permission group can view the status of this VHD
create table vhd_perm_view_status
(
    vhd_id   bigint not null references vhds on delete cascade,
    group_id uuid   not null references groups on delete cascade
);
--- whether _superusers_ of a certain permission group can edit the tags of this VHD
create table vhd_perm_edit_tags
(
    vhd_id   bigint not null references vhds on delete cascade,
    group_id uuid   not null references groups on delete cascade
);

-- MAINTENANCE ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

-- Scheduled system maintenance intervals; so that the system can schedule around updates, etc.
create table maintenance_schedule
(
    begins_at           timestamp without time zone not null,
    ends_at             timestamp without time zone not null,
    reason              text                        not null,
    created_by_admin_id uuid                        not null references tml_admins (admin_id)
);

-- WEB SECURITY ---------------- ---------------- ---------------- ---------------- ---------------- ----------------

-- CSRF prevention / mitigation

create type fingerprint as
(
    user_agent text,
    remote_ip  inet
);

create table user_pre_sessions
(
    pre_session_id uuid primary key            not null,
    fingerprint    fingerprint                 not null,
    csrf_token     text                        not null,
    created_at     timestamp without time zone not null
);
create table user_sessions
(
    session_id  uuid primary key            not null,
    fingerprint fingerprint                 not null,
    csrf_token  text                        not null,
    created_at  timestamp without time zone not null,

    user_id     uuid                        not null references users
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
    job_id            uuid                        not null references jobs (job_id),
    vhd_id            bigint                      not null references vhds (vhd_id) on delete no action,
    run_number        integer                     not null,

    status            text                        not null,
    result            text,
    output            bytea,

    enqueued_at       timestamp without time zone not null,
    started_setup_at  timestamp without time zone not null,
    started_runner_at timestamp without time zone not null,
    terminated_at     timestamp without time zone not null
);
