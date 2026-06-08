-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs" ADD COLUMN "cancel_requested_at" timestamptz NULL;
