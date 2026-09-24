-- Hand-written: until now only supervisor errors ever wrote `exit_message`, so
-- every recorded one moves to `job_error`.
ALTER TABLE "tml_switchboard"."jobs"
ADD COLUMN "job_error" text NULL;


UPDATE "tml_switchboard"."jobs"
SET
    job_error = exit_message,
    exit_message = NULL
WHERE
    exit_message IS NOT NULL
    AND job_state = 'finalized';


ALTER TABLE "tml_switchboard"."jobs"
ADD CONSTRAINT "job_error_only_when_finalized" CHECK (
    (job_error IS NULL)
    OR (
        job_state = 'finalized'::tml_switchboard.job_state
    )
);
