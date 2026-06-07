-- Create index "jobs_queued_idx" to table: "jobs"
CREATE INDEX "jobs_queued_idx" ON "tml_switchboard"."jobs" ("queued_at") WHERE (job_state = 'queued'::tml_switchboard.job_state);

-- Add the `eligible_hosts(job_id, liveness_cutoff)` scheduler helper.
--
-- Atlas's community inspector does not manage Postgres functions, so this object
-- is appended by hand (like `principals` and the grant/acyclicity triggers) and
-- left untouched by future `atlas migrate diff` runs. See SCHEMA.sql for the
-- full rationale: it returns the hosts a job may be dispatched onto by the
-- set-based criteria (idle + live heartbeat + host-tag containment), the cheap
-- pre-filter the scheduler layers DUT matching and image resolution on top of.
--
-- TODO(authz): does not yet restrict to hosts the job's principal may use
-- (ownership / `start` grant via `principals()`); fold that in here later.
CREATE OR REPLACE FUNCTION "tml_switchboard"."eligible_hosts"(
    "p_job_id" uuid,
    "p_liveness_cutoff" timestamp with time zone
)
    RETURNS SETOF uuid
    LANGUAGE sql
    STABLE AS
$$
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
