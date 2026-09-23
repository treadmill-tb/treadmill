-- Hand-written: resumed jobs gain their chain's image reference and resolved
-- image, and lose their restart budget, before the new checks apply.
ALTER TABLE "tml_switchboard"."jobs"
DROP CONSTRAINT "valid_init_spec";


WITH RECURSIVE
    chain (job_id, ancestor_id) AS (
        SELECT
            job_id,
            resume_job_id
        FROM
            "tml_switchboard"."jobs"
        WHERE
            resume_job_id IS NOT NULL
        UNION ALL
        SELECT
            c.job_id,
            a.resume_job_id
        FROM
            chain c
            JOIN "tml_switchboard"."jobs" a ON a.job_id = c.ancestor_id
        WHERE
            a.resume_job_id IS NOT NULL
    )
UPDATE "tml_switchboard"."jobs" j
SET
    image_id = root.image_id,
    image_set_id = root.image_set_id,
    image_set_generation = root.image_set_generation,
    resolved_image_id = root.resolved_image_id
FROM
    chain c
    JOIN "tml_switchboard"."jobs" root ON root.job_id = c.ancestor_id
WHERE
    j.job_id = c.job_id
    AND root.resume_job_id IS NULL;


UPDATE "tml_switchboard"."jobs"
SET
    restart_policy.remaining_restart_count = 0
WHERE
    resume_job_id IS NOT NULL;


ALTER TABLE "tml_switchboard"."jobs"
ADD CONSTRAINT "valid_init_spec" CHECK (
    (
        (
            ((image_id IS NOT NULL))::integer + ((image_set_id IS NOT NULL))::integer
        ) = 1
    )
    AND (
        (image_set_id IS NULL) = (image_set_generation IS NULL)
    )
    AND (
        (resume_job_id IS NULL)
        OR (restart_job_id IS NULL)
    )
),
ADD CONSTRAINT "resume_never_restarts" CHECK (
    (resume_job_id IS NULL)
    OR ((restart_policy).remaining_restart_count = 0)
);
