-- Modify "hosts" table
ALTER TABLE "tml_switchboard"."hosts" ADD COLUMN "last_seen_at" timestamptz NULL;
