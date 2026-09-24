-- Hand-written: `user_id` is renamed rather than dropped and re-added, and
-- every existing job gets its subject row.
ALTER TABLE "tml_switchboard"."subjects"
ADD CONSTRAINT "subjects_subject_id_kind_key" UNIQUE ("subject_id", "kind");


INSERT INTO
    "tml_switchboard"."subjects" (subject_id, kind)
SELECT
    job_id,
    'job'
FROM
    "tml_switchboard"."jobs";


ALTER TABLE "tml_switchboard"."jobs"
ADD CONSTRAINT "jobs_job_id_fkey" FOREIGN KEY ("job_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE;


ALTER TABLE "tml_switchboard"."api_tokens"
DROP CONSTRAINT "api_tokens_user_id_fkey";


ALTER TABLE "tml_switchboard"."api_tokens"
RENAME COLUMN "user_id" TO "subject_id";


ALTER TABLE "tml_switchboard"."api_tokens"
ADD COLUMN "subject_kind" "tml_switchboard"."subject_kind" NOT NULL DEFAULT 'user';


ALTER TABLE "tml_switchboard"."api_tokens"
ALTER COLUMN "subject_kind"
DROP DEFAULT,
ALTER COLUMN "expires_at"
DROP NOT NULL,
ADD CONSTRAINT "api_tokens_subject_id_subject_kind_fkey" FOREIGN KEY ("subject_id", "subject_kind") REFERENCES "tml_switchboard"."subjects" ("subject_id", "kind") ON UPDATE NO ACTION ON DELETE CASCADE,
ADD CONSTRAINT "token_subject_kind" CHECK (
    subject_kind = ANY (
        ARRAY[
            'user'::tml_switchboard.subject_kind,
            'job'::tml_switchboard.subject_kind
        ]
    )
),
ADD CONSTRAINT "job_tokens_never_expire" CHECK (
    (
        subject_kind = 'job'::tml_switchboard.subject_kind
    ) = (expires_at IS NULL)
);
