CREATE SCHEMA tml_switchboard;


CREATE TYPE tml_switchboard.user_type AS enum('normal', 'system');


CREATE TABLE tml_switchboard.users (
    user_id UUID NOT NULL PRIMARY KEY,
    name text NOT NULL UNIQUE,
    email text NOT NULL UNIQUE,
    password_hash text NOT NULL,
    user_type tml_switchboard.user_type NOT NULL,
    locked bool NOT NULL DEFAULT FALSE
);


CREATE TYPE tml_switchboard.api_token_cancellation AS (
    canceled_at TIMESTAMP WITH TIME ZONE,
    cancellation_reason text
);


CREATE TABLE tml_switchboard.api_tokens (
    token_id UUID NOT NULL PRIMARY KEY,
    token bytea NOT NULL UNIQUE,
    user_id UUID NOT NULL REFERENCES tml_switchboard.users (user_id) ON DELETE cascade,
    inherits_user_permissions bool NOT NULL,
    canceled tml_switchboard.api_token_cancellation,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL,
    expires_at TIMESTAMP WITH TIME ZONE NOT NULL,
    CHECK (octet_length(token) = 128)
);


CREATE TABLE tml_switchboard.user_privileges (
    user_id UUID NOT NULL REFERENCES tml_switchboard.users (user_id) ON DELETE cascade,
    permission text NOT NULL,
    PRIMARY KEY (user_id, permission)
);


CREATE TABLE tml_switchboard.api_token_privileges (
    token_id UUID NOT NULL REFERENCES tml_switchboard.api_tokens (token_id) ON DELETE cascade,
    permission text NOT NULL,
    PRIMARY KEY (token_id, permission)
);


CREATE TABLE tml_switchboard.supervisors (
    supervisor_id UUID NOT NULL PRIMARY KEY,
    name text NOT NULL,
    auth_token bytea NOT NULL UNIQUE,
    tags TEXT[] NOT NULL,
    CHECK (octet_length(auth_token) = 128)
);


CREATE TYPE tml_switchboard.restart_policy AS (remaining_restart_count integer);


CREATE TYPE tml_switchboard.functional_state AS enum('queued', 'dispatched', 'finalized');


CREATE TYPE tml_switchboard.exit_status AS enum(
    'supervisor_match_error',
    'internal_supervisor_error',
    'supervisor_host_start_error',
    'supervisor_dropped_job',
    'queue_timeout',
    'job_timeout',
    'job_canceled',
    'workload_finished_success',
    'workload_finished_error',
    'workload_finished_unknown'
);


CREATE TABLE tml_switchboard.jobs (
    job_id UUID NOT NULL PRIMARY KEY,
    resume_job_id UUID REFERENCES tml_switchboard.jobs (job_id) ON DELETE no action,
    restart_job_id UUID REFERENCES tml_switchboard.jobs (job_id) ON DELETE no action,
    image_id bytea,
    ssh_keys TEXT[] NOT NULL,
    restart_policy tml_switchboard.restart_policy NOT NULL,
    enqueued_by_token_id UUID NOT NULL REFERENCES tml_switchboard.api_tokens (token_id) ON DELETE no action,
    tag_config text NOT NULL,
    job_timeout interval NOT NULL,
    functional_state tml_switchboard.functional_state NOT NULL,
    queued_at TIMESTAMP WITH TIME ZONE NOT NULL,
    started_at TIMESTAMP WITH TIME ZONE,
    dispatched_on_supervisor_id UUID,
    exit_status tml_switchboard.exit_status,
    host_output text,
    terminated_at TIMESTAMP WITH TIME ZONE,
    last_updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT valid_init_spec CHECK (
        (
            resume_job_id IS NULL
            AND (image_id IS NOT NULL)
        )
        OR (
            resume_job_id IS NOT NULL
            AND restart_job_id IS NULL
            AND image_id IS NULL
        )
    ),
    CONSTRAINT valid_image_id CHECK (
        CASE
            WHEN image_id IS NOT NULL THEN octet_length(image_id) = 32
            ELSE TRUE
        END
    ),
    CONSTRAINT valid_queued_implication CHECK (
        functional_state != 'queued'
        OR (
            started_at IS NULL
            AND dispatched_on_supervisor_id IS NULL
        )
    ),
    CONSTRAINT valid_dispatched_implication CHECK (
        functional_state != 'dispatched'
        OR (
            started_at IS NOT NULL
            AND dispatched_on_supervisor_id IS NOT NULL
        )
    ),
    CONSTRAINT valid_restart_policy CHECK ((restart_policy).remaining_restart_count >= 0),
    CONSTRAINT exit_status_allows_host_output CHECK (
        CASE
            WHEN exit_status = 'supervisor_match_error' THEN host_output IS NULL
            WHEN exit_status = 'queue_timeout' THEN host_output IS NULL
            WHEN exit_status = 'job_timeout' THEN host_output IS NULL
            WHEN exit_status = 'job_canceled' THEN host_output IS NULL
            WHEN exit_status = 'supervisor_dropped_job' THEN host_output IS NULL
            WHEN exit_status IS NULL THEN host_output IS NULL
            ELSE TRUE
        END
    ),
    CONSTRAINT exit_status_nullity_iso_finalized CHECK (
        (exit_status IS NOT NULL) = (functional_state = 'finalized')
    )
);


CREATE TYPE tml_switchboard.parameter_value AS (value text, is_secret bool);


CREATE TABLE tml_switchboard.job_parameters (
    job_id UUID NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE cascade,
    key text NOT NULL,
    value tml_switchboard.parameter_value NOT NULL,
    PRIMARY KEY (job_id, key)
);


CREATE TABLE tml_switchboard.job_events (
    job_id UUID NOT NULL REFERENCES tml_switchboard.jobs (job_id) ON DELETE cascade,
    job_event jsonb NOT NULL,
    logged_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (
        job_event ? 'event_type'
        AND (
            job_event ->> 'event_type' = 'state_transition'
            OR job_event ->> 'event_type' = 'declare_workload_exit_status'
            OR job_event ->> 'event_type' = 'set_exit_status'
            OR job_event ->> 'event_type' = 'finalize_result'
        )
    )
);
