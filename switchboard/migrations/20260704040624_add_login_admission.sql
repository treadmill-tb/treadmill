-- Modify "users" table
ALTER TABLE "tml_switchboard"."users"
ADD COLUMN "tos_accepted_version" integer NULL,
ADD COLUMN "tos_accepted_at" timestamptz NULL;


-- Create "login_allowlist" table
CREATE TABLE "tml_switchboard"."login_allowlist" (
    "provider" text NOT NULL,
    "kind" text NOT NULL,
    "external_id" text NOT NULL,
    "comment" text NULL,
    "added_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("provider", "kind", "external_id"),
    CONSTRAINT "valid_allowlist_kind" CHECK (kind = ANY (ARRAY['user'::text, 'org'::text]))
);


-- Create "pending_registrations" table
CREATE TABLE "tml_switchboard"."pending_registrations" (
    "id" uuid NOT NULL,
    "provider" text NOT NULL,
    "identity" jsonb NULL,
    "existing_user_id" uuid NULL,
    "org_ids" TEXT[] NOT NULL DEFAULT '{}',
    "created_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "expires_at" timestamptz NOT NULL,
    PRIMARY KEY ("id"),
    CONSTRAINT "pending_registrations_existing_user_id_fkey" FOREIGN KEY ("existing_user_id") REFERENCES "tml_switchboard"."users" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE,
    CONSTRAINT "pending_kind" CHECK ((identity IS NULL) <> (existing_user_id IS NULL))
);


-- Seed the anonymous actor (Atlas does not diff data inserts; added by hand).
INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000003', 'system');
