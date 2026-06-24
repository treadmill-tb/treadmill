CREATE SCHEMA tml_switchboard;


CREATE TYPE tml_switchboard.subject_kind AS enum('user', 'group', 'system');


CREATE TABLE tml_switchboard.subjects (
    subject_id uuid NOT NULL PRIMARY KEY,
    kind tml_switchboard.subject_kind NOT NULL
);


CREATE TABLE tml_switchboard.users (
    subject_id uuid NOT NULL PRIMARY KEY REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    username text NOT NULL UNIQUE,
    full_name text,
    avatar_url text,
    locked bool NOT NULL DEFAULT FALSE
);


CREATE TABLE tml_switchboard.user_identities (
    provider text NOT NULL,
    provider_user_id text NOT NULL,
    user_id uuid NOT NULL REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    provider_login text,
    linked_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (provider, provider_user_id),
    UNIQUE (user_id, provider)
);


CREATE TABLE tml_switchboard.user_emails (
    email text NOT NULL PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    provider text NOT NULL,
    verified bool NOT NULL,
    added_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    CHECK (verified)
);


CREATE TABLE tml_switchboard.groups (
    subject_id uuid NOT NULL PRIMARY KEY REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    name text NOT NULL UNIQUE
);


INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000001', 'group');


INSERT INTO
    tml_switchboard.groups (subject_id, name)
VALUES
    ('00000000-0000-0000-0000-000000000001', 'admins');


INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000002', 'system');


CREATE TYPE tml_switchboard.membership_source AS enum('manual', 'github_org');


CREATE TABLE tml_switchboard.group_members (
    group_id uuid NOT NULL REFERENCES tml_switchboard.groups (subject_id) ON DELETE CASCADE,
    member_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    source tml_switchboard.membership_source NOT NULL,
    source_ref text NOT NULL DEFAULT '',
    PRIMARY KEY (group_id, member_id, source, source_ref),
    CONSTRAINT no_self_membership CHECK (group_id <> member_id)
);


CREATE INDEX group_members_member_id_idx ON tml_switchboard.group_members (member_id);


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


CREATE CONSTRAINT TRIGGER group_members_acyclic
AFTER insert
OR
UPDATE ON tml_switchboard.group_members FOR each ROW
EXECUTE function tml_switchboard.group_members_no_cycle ();


CREATE TABLE tml_switchboard.group_auto_sources (
    group_id uuid NOT NULL REFERENCES tml_switchboard.groups (subject_id) ON DELETE CASCADE,
    provider text NOT NULL,
    external_id text NOT NULL,
    external_name text,
    membership_via tml_switchboard.membership_source NOT NULL,
    last_synced_at timestamp with time zone,
    PRIMARY KEY (group_id, provider, external_id)
);


CREATE TYPE tml_switchboard.api_token_revocation AS (
    revoked_at timestamp with time zone,
    revocation_reason text
);


CREATE TABLE tml_switchboard.api_tokens (
    token_id uuid NOT NULL PRIMARY KEY,
    token bytea NOT NULL UNIQUE,
    user_id uuid NOT NULL REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    revoked tml_switchboard.api_token_revocation,
    created_at timestamp with time zone NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    user_agent text,
    comment text,
    created_ip text,
    created_port integer,
    CHECK (octet_length(token) = 32)
);


CREATE TABLE tml_switchboard.oauth_flows (
    state text NOT NULL PRIMARY KEY,
    provider text NOT NULL,
    pkce_verifier text,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    expires_at timestamp with time zone NOT NULL
);


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


CREATE TABLE tml_switchboard.hosts (
    host_id uuid NOT NULL PRIMARY KEY,
    name text NOT NULL,
    auth_token bytea NOT NULL UNIQUE,
    tags TEXT[] NOT NULL,
    owner_id uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    ssh_endpoints tml_switchboard.ssh_endpoint[] NOT NULL,
    current_job uuid,
    worker_instance_id bigint NOT NULL DEFAULT 0,
    last_seen_at timestamp with time zone,
    CHECK (octet_length(auth_token) = 32),
    CHECK (worker_instance_id >= 0)
);


CREATE TYPE tml_switchboard.restart_policy AS (remaining_restart_count integer);


CREATE TYPE tml_switchboard.job_state AS enum(
    'queued',
    'assigned',
    'initializing',
    'ready',
    'terminating',
    'finalized'
);


CREATE TYPE tml_switchboard.job_initializing_stage AS enum(
    'starting',
    'fetching_image',
    'allocating',
    'provisioning',
    'booting'
);


CREATE TYPE tml_switchboard.termination_reason AS enum(
    'workload_exited',
    'workload_self_canceled',
    'user_canceled',
    'queue_timeout',
    'execution_timeout',
    'image_error',
    'host_match_error',
    'host_start_failure',
    'host_dropped_job',
    'host_unreachable',
    'resume_failed',
    'internal_error'
);


CREATE TYPE tml_switchboard.task_exit_status AS enum('pending', 'success', 'failure');


CREATE TABLE tml_switchboard.jobs (
    job_id uuid NOT NULL PRIMARY KEY,
    owner_id uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    resume_job_id uuid REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION,
    restart_job_id uuid REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION,
    image_id uuid,
    image_group_id uuid,
    image_group_generation int,
    resolved_image_id uuid,
    ssh_keys TEXT[] NOT NULL,
    restart_policy tml_switchboard.restart_policy NOT NULL,
    enqueued_by_token_id uuid NOT NULL REFERENCES tml_switchboard.api_tokens (token_id) ON DELETE NO ACTION,
    host_tag_requirements TEXT[] NOT NULL DEFAULT '{}',
    job_timeout interval NOT NULL,
    job_state tml_switchboard.job_state NOT NULL,
    initializing_stage tml_switchboard.job_initializing_stage,
    queued_at timestamp with time zone NOT NULL,
    started_at timestamp with time zone,
    dispatched_on_host_id uuid,
    ssh_endpoints tml_switchboard.ssh_endpoint[],
    cancel_requested_at timestamp with time zone,
    termination_reason tml_switchboard.termination_reason,
    task_exit_status tml_switchboard.task_exit_status,
    exit_message text,
    terminated_at timestamp with time zone,
    last_updated_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    CONSTRAINT valid_init_spec CHECK (
        (
            resume_job_id IS NULL
            AND (image_id IS NOT NULL)::int + (image_group_id IS NOT NULL)::int = 1
            AND (image_group_id IS NULL) = (image_group_generation IS NULL)
        )
        OR (
            resume_job_id IS NOT NULL
            AND restart_job_id IS NULL
            AND image_id IS NULL
            AND image_group_id IS NULL
            AND image_group_generation IS NULL
        )
    ),
    CONSTRAINT valid_restart_policy CHECK ((restart_policy).remaining_restart_count >= 0),
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
    CONSTRAINT started_at_iso_executing CHECK (
        (started_at IS NOT NULL) = (
            job_state IN ('initializing', 'ready', 'terminating')
        )
    ),
    CONSTRAINT termination_reason_iso_finalized CHECK (
        (termination_reason IS NOT NULL) = (job_state = 'finalized')
    ),
    CONSTRAINT terminated_at_iso_finalized CHECK (
        (terminated_at IS NOT NULL) = (job_state = 'finalized')
    ),
    CONSTRAINT task_exit_status_absent_while_queued CHECK (
        task_exit_status IS NULL
        OR job_state <> 'queued'
    ),
    CONSTRAINT initializing_stage_iso_initializing CHECK (
        (initializing_stage IS NOT NULL) = (job_state = 'initializing')
    )
);


ALTER TABLE tml_switchboard.hosts
ADD FOREIGN KEY (current_job) REFERENCES tml_switchboard.jobs (job_id) ON DELETE NO ACTION;


CREATE UNIQUE INDEX hosts_current_job_unique ON tml_switchboard.hosts (current_job)
WHERE
    current_job IS NOT NULL;


CREATE INDEX hosts_tags_gin ON tml_switchboard.hosts USING gin (tags);


CREATE TABLE tml_switchboard.host_targets (
    target_id uuid NOT NULL PRIMARY KEY,
    host_id uuid NOT NULL REFERENCES tml_switchboard.hosts (host_id) ON DELETE CASCADE,
    name text NOT NULL,
    tags TEXT[] NOT NULL DEFAULT '{}',
    UNIQUE (host_id, name)
);


CREATE INDEX host_targets_tags_gin ON tml_switchboard.host_targets USING gin (tags);


CREATE OR REPLACE FUNCTION tml_switchboard.principals (arg_subject uuid) returns TABLE (id uuid) language sql stable AS $$
    with recursive reached(id) as (
        select arg_subject
        union
        select gm.group_id
        from tml_switchboard.group_members gm
        join reached r on gm.member_id = r.id
    )
    select id from reached;
$$;


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


CREATE TYPE tml_switchboard.parameter_value AS (value text, is_secret bool);


CREATE TABLE tml_switchboard.job_parameters (
    job_id uuid NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE CASCADE,
    key text NOT NULL,
    value tml_switchboard.parameter_value NOT NULL,
    PRIMARY KEY (job_id, key)
);


CREATE TABLE tml_switchboard.job_target_requirements (
    job_id uuid NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE CASCADE,
    req_index int NOT NULL,
    tags TEXT[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (job_id, req_index)
);


CREATE INDEX jobs_queued_idx ON tml_switchboard.jobs (queued_at)
WHERE
    job_state = 'queued';


CREATE INDEX jobs_queued_at_job_id_idx ON tml_switchboard.jobs (queued_at DESC, job_id DESC);


CREATE FUNCTION tml_switchboard.eligible_hosts (
    p_job_id uuid,
    p_liveness_cutoff timestamp with time zone
) returns setof uuid language sql stable AS $$
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


CREATE TABLE tml_switchboard.images (
    id uuid NOT NULL PRIMARY KEY,
    manifest_digest text NOT NULL UNIQUE,
    artifact_type text NOT NULL,
    owner_subject uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    label text,
    attrs jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


CREATE TABLE tml_switchboard.image_locations (
    image_id uuid NOT NULL REFERENCES tml_switchboard.images (id) ON DELETE CASCADE,
    registry text NOT NULL,
    repository text NOT NULL,
    status text NOT NULL,
    added_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (image_id, registry, repository),
    CONSTRAINT valid_location_status CHECK (status IN ('external', 'canonical', 'system'))
);


CREATE TABLE tml_switchboard.image_groups (
    id uuid NOT NULL PRIMARY KEY,
    name text NOT NULL UNIQUE,
    owner_subject uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    label text,
    public boolean NOT NULL DEFAULT FALSE,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


CREATE TABLE tml_switchboard.image_group_generations (
    group_id uuid NOT NULL REFERENCES tml_switchboard.image_groups (id) ON DELETE CASCADE,
    generation int NOT NULL,
    created_by uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (group_id, generation),
    CONSTRAINT generation_positive CHECK (generation >= 1)
);


CREATE TABLE tml_switchboard.image_group_members (
    group_id uuid NOT NULL,
    generation int NOT NULL,
    image_id uuid NOT NULL REFERENCES tml_switchboard.images (id),
    required_host_tags TEXT[] NOT NULL DEFAULT '{}',
    index int NOT NULL,
    PRIMARY KEY (group_id, generation, image_id),
    UNIQUE (group_id, generation, index),
    FOREIGN key (group_id, generation) REFERENCES tml_switchboard.image_group_generations (group_id, generation) ON DELETE CASCADE
);


CREATE OR REPLACE FUNCTION tml_switchboard.deny_generation_change () returns trigger language plpgsql AS $$
begin
    raise exception 'image-group generations are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;


CREATE TRIGGER image_group_generations_append_only before
UPDATE
OR delete ON tml_switchboard.image_group_generations FOR each ROW
EXECUTE function tml_switchboard.deny_generation_change ();


CREATE TYPE tml_switchboard.image_group_permission AS enum('use', 'manage');


CREATE TABLE tml_switchboard.image_group_grants (
    group_id uuid NOT NULL REFERENCES tml_switchboard.image_groups (id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    permission tml_switchboard.image_group_permission NOT NULL,
    granted_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (group_id, subject_id, permission)
);


ALTER TABLE tml_switchboard.jobs
ADD FOREIGN KEY (image_id) REFERENCES tml_switchboard.images (id) ON DELETE NO ACTION;


ALTER TABLE tml_switchboard.jobs
ADD FOREIGN KEY (resolved_image_id) REFERENCES tml_switchboard.images (id) ON DELETE NO ACTION;


ALTER TABLE tml_switchboard.jobs
ADD FOREIGN KEY (image_group_id, image_group_generation) REFERENCES tml_switchboard.image_group_generations (group_id, generation) ON DELETE NO ACTION;


CREATE TABLE tml_switchboard.audit_events (
    event_id uuid NOT NULL PRIMARY KEY,
    event_type text NOT NULL,
    payload jsonb NOT NULL,
    actor_id uuid NOT NULL,
    correlation_id uuid,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp
);


CREATE INDEX audit_events_created_at_idx ON tml_switchboard.audit_events (created_at);


CREATE TYPE tml_switchboard.audit_entity_kind AS enum('job', 'host', 'subject');


CREATE TYPE tml_switchboard.audit_role AS enum('actor', 'subject', 'context');


CREATE TABLE tml_switchboard.audit_event_relations (
    event_id uuid NOT NULL REFERENCES tml_switchboard.audit_events (event_id) ON DELETE CASCADE,
    entity_kind tml_switchboard.audit_entity_kind NOT NULL,
    entity_id uuid NOT NULL,
    role tml_switchboard.audit_role NOT NULL,
    view_policy text NOT NULL,
    PRIMARY KEY (event_id, entity_kind, entity_id, role)
);


CREATE INDEX audit_event_relations_entity_idx ON tml_switchboard.audit_event_relations (entity_kind, entity_id, event_id DESC);


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
