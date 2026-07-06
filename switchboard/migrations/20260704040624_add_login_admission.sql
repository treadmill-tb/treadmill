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


-- Modify "oauth_flows" table
ALTER TABLE "tml_switchboard"."oauth_flows"
ADD COLUMN "return_to" text NULL;


-- Create "staged_logins" table
CREATE TABLE tml_switchboard.staged_logins (
    id uuid NOT NULL PRIMARY KEY,
    secret_hash text NOT NULL,
    provider text NOT NULL,
    identity jsonb,
    existing_user_id uuid REFERENCES tml_switchboard.users (subject_id) ON DELETE CASCADE,
    org_ids TEXT[] NOT NULL DEFAULT '{}',
    return_to text,
    user_agent text,
    created_ip text,
    created_port integer,
    created_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    expires_at timestamp with time zone NOT NULL,
    CONSTRAINT staged_kind CHECK ((identity IS NULL) <> (existing_user_id IS NULL))
);


-- Seed the anonymous actor (Atlas does not diff data inserts; added by hand).
INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000003', 'system');
