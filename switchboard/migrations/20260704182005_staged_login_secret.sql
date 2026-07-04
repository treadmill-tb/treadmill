-- Modify "staged_logins" table
ALTER TABLE "tml_switchboard"."staged_logins"
ADD COLUMN "secret_hash" text NOT NULL;
