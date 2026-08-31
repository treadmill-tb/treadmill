-- Modify "hosts" table
ALTER TABLE "tml_switchboard"."hosts"
ADD COLUMN "maintenance" boolean NOT NULL DEFAULT FALSE;


-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
ADD COLUMN "host_cel_predicate" text NOT NULL DEFAULT 'true';


-- Create "host_specs" table
CREATE TABLE "tml_switchboard"."host_specs" (
    "host_id" uuid NOT NULL,
    "revision" integer NOT NULL,
    "spec" jsonb NOT NULL,
    "spec_version" text NOT NULL,
    "written_by" uuid NULL,
    "written_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("host_id", "revision"),
    CONSTRAINT "host_specs_host_id_fkey" FOREIGN KEY ("host_id") REFERENCES "tml_switchboard"."hosts" ("host_id") ON UPDATE NO ACTION ON DELETE CASCADE,
    CONSTRAINT "host_specs_written_by_fkey" FOREIGN KEY ("written_by") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE SET NULL,
    CONSTRAINT "revision_positive" CHECK (revision >= 1),
    CONSTRAINT "spec_id_matches" CHECK (((spec ->> 'id'::text))::uuid = host_id)
);


-- Atlas (community) does not diff functions or triggers; the rest of this
-- migration is hand-written to match SCHEMA.sql.
-- Append-only enforcement for host_specs.
CREATE OR REPLACE FUNCTION tml_switchboard.deny_host_spec_change () returns trigger language plpgsql AS $$
begin
    raise exception 'host specs are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;


CREATE TRIGGER host_specs_append_only BEFORE
UPDATE
OR DELETE ON "tml_switchboard"."host_specs" FOR EACH ROW
EXECUTE FUNCTION tml_switchboard.deny_host_spec_change ();


-- Wake the scheduler and the host watch endpoints on a spec edit; spec writes
-- never touch `hosts`, so its triggers do not cover them.
CREATE TRIGGER host_specs_notify_write
AFTER INSERT ON "tml_switchboard"."host_specs" FOR EACH ROW
EXECUTE FUNCTION tml_switchboard.notify_change ('host_id');


-- Exclude hosts in maintenance from both scheduling and preemption.
CREATE OR REPLACE FUNCTION tml_switchboard.eligible_hosts (
    p_job_id uuid,
    p_liveness_cutoff timestamp with time zone
) returns setof uuid language sql stable AS $$
    select h.host_id
    from tml_switchboard.hosts h
    join tml_switchboard.job_authorized_hosts(p_job_id) a on a = h.host_id
    where h.current_job is null
      and not h.maintenance
      and h.last_seen_at is not null
      and h.last_seen_at > p_liveness_cutoff
      and h.tags @> (
          select host_tag_requirements
          from tml_switchboard.jobs
          where job_id = p_job_id
      )
    order by h.host_id;
$$;


CREATE OR REPLACE FUNCTION tml_switchboard.reclaimable_hosts (
    p_job_id uuid,
    p_liveness_cutoff timestamp with time zone,
    p_now timestamp with time zone
) returns TABLE (host_id uuid, reclaim_pending boolean) language sql stable AS $$
    select h.host_id, v.terminate_requested_at is not null
    from tml_switchboard.hosts h
    join tml_switchboard.jobs v on v.job_id = h.current_job
    join tml_switchboard.job_authorized_hosts(p_job_id) a on a = h.host_id
    where not h.maintenance
      and h.last_seen_at is not null
      and h.last_seen_at > p_liveness_cutoff
      and h.tags @> (
          select host_tag_requirements
          from tml_switchboard.jobs
          where job_id = p_job_id
      )
      and v.job_state <> 'finalized'
      and v.lease_expiry_action = 'preempt'
      and v.started_at is not null
      and v.started_at + v.lease_duration <= p_now
    order by v.started_at + v.lease_duration;
$$;
