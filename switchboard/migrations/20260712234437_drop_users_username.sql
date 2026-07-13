-- Replace the separate `username` handle and optional `full_name` with a
-- single user-chosen display name. Backfill it from the display name, trimmed,
-- falling back to the login handle when that is absent or blank, before
-- enforcing non-emptiness.
ALTER TABLE "tml_switchboard"."users"
ADD COLUMN "name" text;


UPDATE "tml_switchboard"."users"
SET
    "name" = coalesce(nullif(btrim("full_name"), ''), "username");


ALTER TABLE "tml_switchboard"."users"
ALTER COLUMN "name"
SET NOT NULL,
ADD CONSTRAINT "valid_name" CHECK (
    (name <> ''::text)
    AND (char_length(name) <= 256)
),
DROP COLUMN "username",
DROP COLUMN "full_name";
