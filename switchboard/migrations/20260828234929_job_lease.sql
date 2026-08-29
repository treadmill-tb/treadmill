-- Create enum type "lease_expiry_action"
CREATE TYPE "tml_switchboard"."lease_expiry_action" AS ENUM('terminate', 'preempt');


-- Modify "jobs" table. `job_timeout` is renamed rather than dropped/re-added:
-- same meaning, and existing rows keep their lease.
ALTER TABLE "tml_switchboard"."jobs"
RENAME COLUMN "job_timeout" TO "lease_duration";


ALTER TABLE "tml_switchboard"."jobs"
ADD COLUMN "lease_expiry_action" "tml_switchboard"."lease_expiry_action" NOT NULL DEFAULT 'terminate',
ADD COLUMN "terminate_requested_reason" "tml_switchboard"."termination_reason" NULL,
ADD CONSTRAINT "lease_duration_non_negative" CHECK (lease_duration >= '00:00:00'::interval),
ADD CONSTRAINT "terminate_request_iso" CHECK (
    (terminate_requested_at IS NULL) = (terminate_requested_reason IS NULL)
),
ADD CONSTRAINT "terminate_requested_reason_valid" CHECK (
    terminate_requested_reason = ANY (
        ARRAY[
            'user_terminated'::tml_switchboard.termination_reason,
            'preempted'::tml_switchboard.termination_reason
        ]
    )
);


-- Existing pending terminate signals are all user-terminates.
UPDATE "tml_switchboard"."jobs"
SET
    "terminate_requested_reason" = 'user_terminated'
WHERE
    "terminate_requested_at" IS NOT NULL;


-- Functions (atlas community does not diff these).
CREATE FUNCTION tml_switchboard.job_authorized_hosts (p_job_id uuid) returns setof uuid language sql stable AS $$
    with owner_principals (id) as (
        select p.id
        from tml_switchboard.jobs j, tml_switchboard.principals(j.owner_id) p
        where j.job_id = p_job_id
    )
    select h.host_id
    from tml_switchboard.hosts h
    where exists (
              select 1 from owner_principals
              where id = '00000000-0000-0000-0000-000000000001'
          )
       or exists (
              select 1 from owner_principals op where op.id = h.owner_id
          )
       or exists (
              select 1
              from tml_switchboard.host_grants g
              join owner_principals op on g.subject_id = op.id
              where g.host_id = h.host_id and g.permission = 'start'
          );
$$;


CREATE OR REPLACE FUNCTION tml_switchboard.eligible_hosts (
    p_job_id uuid,
    p_liveness_cutoff timestamp with time zone
) returns setof uuid language sql stable AS $$
    select h.host_id
    from tml_switchboard.hosts h
    join tml_switchboard.job_authorized_hosts(p_job_id) a on a = h.host_id
    where h.current_job is null
      and h.last_seen_at is not null
      and h.last_seen_at > p_liveness_cutoff
      and h.tags @> (
          select host_tag_requirements
          from tml_switchboard.jobs
          where job_id = p_job_id
      )
    order by h.host_id;
$$;
