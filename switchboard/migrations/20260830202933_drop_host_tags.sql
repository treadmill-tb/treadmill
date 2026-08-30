-- Atlas (community) does not diff functions; these drop the host-tag
-- containment clause. Eligibility beyond the set logic is now the
-- application's: the CEL predicate and image-set member match run in Rust.
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
      and v.job_state <> 'finalized'
      and v.lease_expiry_action = 'preempt'
      and v.started_at is not null
      and v.started_at + v.lease_duration <= p_now
    order by v.started_at + v.lease_duration;
$$;


-- Modify "hosts" table
-- `hosts_notify_update` compares whole rows in its WHEN clause, which Postgres
-- records as a dependency on every column, so it blocks the drop. Recreate it
-- afterwards, unchanged.
DROP TRIGGER hosts_notify_update ON "tml_switchboard"."hosts";


ALTER TABLE "tml_switchboard"."hosts"
DROP COLUMN "tags";


CREATE TRIGGER hosts_notify_update
AFTER
UPDATE ON tml_switchboard.hosts FOR each ROW WHEN (
    -- Exclude `last_seen_at` from check
    (to_jsonb(OLD) - 'last_seen_at') IS DISTINCT FROM (to_jsonb(NEW) - 'last_seen_at')
)
EXECUTE FUNCTION tml_switchboard.notify_change ('host_id');


-- Modify "image_set_members" table
-- Members written while tags still existed may carry no profile. There is no
-- way to infer the right one from a tag set, so they get a placeholder that
-- matches no host: such a member was already unreachable via profiles, and an
-- operator must republish the generation to make it selectable again.
UPDATE "tml_switchboard"."image_set_members"
SET
    "platform_profile" = 'unknown'
WHERE
    "platform_profile" IS NULL;


ALTER TABLE "tml_switchboard"."image_set_members"
DROP COLUMN "required_host_tags",
ALTER COLUMN "platform_profile"
SET NOT NULL;


-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
DROP COLUMN "host_tag_requirements";


-- Drop "host_targets" table
DROP TABLE "tml_switchboard"."host_targets";


-- Drop "job_target_requirements" table
DROP TABLE "tml_switchboard"."job_target_requirements";
