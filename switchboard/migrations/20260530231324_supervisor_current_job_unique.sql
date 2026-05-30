-- Create index "supervisors_current_job_unique" to table: "supervisors"
CREATE UNIQUE INDEX "supervisors_current_job_unique" ON "tml_switchboard"."supervisors" ("current_job") WHERE (current_job IS NOT NULL);
