-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
ADD CONSTRAINT "revision_positive" CHECK (revision >= 1),
ADD COLUMN "revision" bigint NOT NULL DEFAULT 1;


-- Create "job_annotations" table
CREATE TABLE "tml_switchboard"."job_annotations" (
    "job_id" uuid NOT NULL,
    "key" text NOT NULL,
    "value" text NOT NULL,
    PRIMARY KEY ("job_id", "key"),
    CONSTRAINT "job_annotations_job_id_fkey" FOREIGN KEY ("job_id") REFERENCES "tml_switchboard"."jobs" ("job_id") ON UPDATE NO ACTION ON DELETE CASCADE,
    CONSTRAINT "valid_annotation_key" CHECK (
        char_length(key) BETWEEN 1 AND 128
        AND key ~ '^[a-z0-9]([a-z0-9._/-]*[a-z0-9])?$'
    ),
    CONSTRAINT "valid_annotation_value" CHECK (
        (char_length(value) <= 1024)
        AND (value !~ '[[:cntrl:]]'::text)
    )
);
