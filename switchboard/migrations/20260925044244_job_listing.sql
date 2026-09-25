-- Drop index "jobs_queued_at_job_id_idx" from table: "jobs"
DROP INDEX "tml_switchboard"."jobs_queued_at_job_id_idx";


-- Create index "jobs_listing_idx" to table: "jobs"
CREATE INDEX "jobs_listing_idx" ON "tml_switchboard"."jobs" (
    (COALESCE(terminated_at, queued_at)) DESC,
    "job_id" DESC
);
