-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
DROP CONSTRAINT "dispatched_host_iso_assigned",
DROP CONSTRAINT "started_at_iso_executing",
ADD CONSTRAINT "dispatched_host_monotonic" CHECK (
    CASE job_state
        WHEN 'queued'::tml_switchboard.job_state THEN (dispatched_on_host_id IS NULL)
        WHEN 'finalized'::tml_switchboard.job_state THEN TRUE
        ELSE (dispatched_on_host_id IS NOT NULL)
    END
),
ADD CONSTRAINT "started_at_monotonic" CHECK (
    CASE job_state
        WHEN 'queued'::tml_switchboard.job_state THEN (started_at IS NULL)
        WHEN 'assigned'::tml_switchboard.job_state THEN (started_at IS NULL)
        WHEN 'finalized'::tml_switchboard.job_state THEN TRUE
        ELSE (started_at IS NOT NULL)
    END
);
