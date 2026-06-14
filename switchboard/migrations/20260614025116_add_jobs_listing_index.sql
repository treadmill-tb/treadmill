-- Create index "jobs_queued_at_job_id_idx" to table: "jobs"
CREATE INDEX "jobs_queued_at_job_id_idx" ON "tml_switchboard"."jobs" ("queued_at" DESC, "job_id" DESC);
