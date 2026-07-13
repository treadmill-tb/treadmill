-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
ADD CONSTRAINT "valid_label" CHECK (
    (label ~ '^[ -~]+$'::text)
    AND (char_length(label) <= 256)
),
ADD COLUMN "label" text NULL;
