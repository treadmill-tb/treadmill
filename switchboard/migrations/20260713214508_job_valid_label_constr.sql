-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
DROP CONSTRAINT "valid_label",
ADD CONSTRAINT "valid_label" CHECK (
    char_length(label) BETWEEN 1 AND 256
    AND label ~ '^[A-Za-z0-9]([A-Za-z0-9 _-]*[A-Za-z0-9])?$'
);
