-- Modify "pending_registrations" table
ALTER TABLE "tml_switchboard"."pending_registrations"
ADD COLUMN "secret_hash" text NOT NULL;
