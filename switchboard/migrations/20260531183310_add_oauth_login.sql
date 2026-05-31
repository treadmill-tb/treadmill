-- Identity, group, and authorization model rewrite, plus OAuth login support.
--
-- This migration is hand-edited: the prior schema's `users` table is referenced
-- by several foreign keys (api_tokens, the now-removed privilege tables), so the
-- auto-generated in-place ALTER could not drop `users_pkey` without first
-- detaching those dependents. There is no production data to preserve, so the
-- legacy users/privilege tables are dropped and rebuilt rather than migrated in
-- place. Atlas's community inspector does not manage Postgres functions,
-- triggers, or seed data, so those objects (the acyclicity trigger, the
-- irrevocable-grant guard, and the seeded `admins` group) are appended here by
-- hand and left untouched by future `atlas migrate diff` runs.

-- ---------------------------------------------------------------------------
-- Tear down the legacy identity/authorization objects (no data to preserve).
-- ---------------------------------------------------------------------------
DROP TABLE "tml_switchboard"."api_token_privileges";
DROP TABLE "tml_switchboard"."user_privileges";
-- CASCADE drops the api_tokens -> users foreign key; api_tokens itself is kept.
DROP TABLE "tml_switchboard"."users" CASCADE;
DROP TYPE "tml_switchboard"."user_type";
ALTER TABLE "tml_switchboard"."api_tokens" DROP COLUMN "inherits_user_permissions";

-- ---------------------------------------------------------------------------
-- Enums.
-- ---------------------------------------------------------------------------
CREATE TYPE "tml_switchboard"."subject_kind" AS ENUM ('user', 'group', 'system');
CREATE TYPE "tml_switchboard"."membership_source" AS ENUM ('manual', 'github_org');
CREATE TYPE "tml_switchboard"."supervisor_permission" AS ENUM ('read', 'start', 'ssh', 'manage');
CREATE TYPE "tml_switchboard"."job_permission" AS ENUM ('read', 'stop', 'ssh', 'manage');

-- ---------------------------------------------------------------------------
-- Subjects, users, groups.
-- ---------------------------------------------------------------------------
CREATE TABLE "tml_switchboard"."subjects" (
  "subject_id" uuid NOT NULL,
  "kind" "tml_switchboard"."subject_kind" NOT NULL,
  PRIMARY KEY ("subject_id")
);

CREATE TABLE "tml_switchboard"."users" (
  "subject_id" uuid NOT NULL,
  "username" text NOT NULL,
  "full_name" text NULL,
  "avatar_url" text NULL,
  "locked" boolean NOT NULL DEFAULT false,
  PRIMARY KEY ("subject_id"),
  CONSTRAINT "users_username_key" UNIQUE ("username"),
  CONSTRAINT "users_subject_id_fkey" FOREIGN KEY ("subject_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);

CREATE TABLE "tml_switchboard"."user_identities" (
  "provider" text NOT NULL,
  "provider_user_id" text NOT NULL,
  "user_id" uuid NOT NULL,
  "provider_login" text NULL,
  "linked_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY ("provider", "provider_user_id"),
  CONSTRAINT "user_identities_user_id_provider_key" UNIQUE ("user_id", "provider"),
  CONSTRAINT "user_identities_user_id_fkey" FOREIGN KEY ("user_id") REFERENCES "tml_switchboard"."users" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);

CREATE TABLE "tml_switchboard"."user_emails" (
  "email" text NOT NULL,
  "user_id" uuid NOT NULL,
  "provider" text NOT NULL,
  "verified" boolean NOT NULL,
  "added_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY ("email"),
  CONSTRAINT "user_emails_user_id_fkey" FOREIGN KEY ("user_id") REFERENCES "tml_switchboard"."users" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE,
  CONSTRAINT "user_emails_verified_check" CHECK (verified)
);

CREATE TABLE "tml_switchboard"."groups" (
  "subject_id" uuid NOT NULL,
  "name" text NOT NULL,
  PRIMARY KEY ("subject_id"),
  CONSTRAINT "groups_name_key" UNIQUE ("name"),
  CONSTRAINT "groups_subject_id_fkey" FOREIGN KEY ("subject_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);

-- The well-known global admin group (hard-coded UUID).
INSERT INTO "tml_switchboard"."subjects" ("subject_id", "kind")
VALUES ('00000000-0000-0000-0000-000000000001', 'group');
INSERT INTO "tml_switchboard"."groups" ("subject_id", "name")
VALUES ('00000000-0000-0000-0000-000000000001', 'admins');

-- ---------------------------------------------------------------------------
-- Group membership (acyclic DAG) and auto-sync sources.
-- ---------------------------------------------------------------------------
CREATE TABLE "tml_switchboard"."group_members" (
  "group_id" uuid NOT NULL,
  "member_id" uuid NOT NULL,
  "source" "tml_switchboard"."membership_source" NOT NULL,
  "source_ref" text NOT NULL DEFAULT '',
  PRIMARY KEY ("group_id", "member_id", "source", "source_ref"),
  CONSTRAINT "group_members_group_id_fkey" FOREIGN KEY ("group_id") REFERENCES "tml_switchboard"."groups" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE,
  CONSTRAINT "group_members_member_id_fkey" FOREIGN KEY ("member_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE,
  CONSTRAINT "no_self_membership" CHECK (group_id <> member_id)
);
CREATE INDEX "group_members_member_id_idx" ON "tml_switchboard"."group_members" ("member_id");

CREATE TABLE "tml_switchboard"."group_auto_sources" (
  "group_id" uuid NOT NULL,
  "provider" text NOT NULL,
  "external_id" text NOT NULL,
  "external_name" text NULL,
  "membership_via" "tml_switchboard"."membership_source" NOT NULL,
  "last_synced_at" timestamptz NULL,
  PRIMARY KEY ("group_id", "provider", "external_id"),
  CONSTRAINT "group_auto_sources_group_id_fkey" FOREIGN KEY ("group_id") REFERENCES "tml_switchboard"."groups" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);

-- ---------------------------------------------------------------------------
-- Re-attach api_tokens to the rebuilt users table, add OAuth flow state.
-- ---------------------------------------------------------------------------
ALTER TABLE "tml_switchboard"."api_tokens"
  ADD CONSTRAINT "api_tokens_user_id_fkey" FOREIGN KEY ("user_id") REFERENCES "tml_switchboard"."users" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE;

CREATE TABLE "tml_switchboard"."oauth_flows" (
  "state" text NOT NULL,
  "provider" text NOT NULL,
  "pkce_verifier" text NULL,
  "created_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
  "expires_at" timestamptz NOT NULL,
  PRIMARY KEY ("state")
);

-- ---------------------------------------------------------------------------
-- Resource ownership and grant ACLs.
-- ---------------------------------------------------------------------------
ALTER TABLE "tml_switchboard"."supervisors"
  ADD COLUMN "owner_id" uuid NULL,
  ADD CONSTRAINT "supervisors_owner_id_fkey" FOREIGN KEY ("owner_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE SET NULL;

ALTER TABLE "tml_switchboard"."jobs"
  ADD COLUMN "owner_id" uuid NULL,
  ADD CONSTRAINT "jobs_owner_id_fkey" FOREIGN KEY ("owner_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE SET NULL;

CREATE TABLE "tml_switchboard"."supervisor_grants" (
  "supervisor_id" uuid NOT NULL,
  "subject_id" uuid NOT NULL,
  "permission" "tml_switchboard"."supervisor_permission" NOT NULL,
  "revocable" boolean NOT NULL DEFAULT true,
  "granted_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY ("supervisor_id", "subject_id", "permission"),
  CONSTRAINT "supervisor_grants_supervisor_id_fkey" FOREIGN KEY ("supervisor_id") REFERENCES "tml_switchboard"."supervisors" ("supervisor_id") ON UPDATE NO ACTION ON DELETE CASCADE,
  CONSTRAINT "supervisor_grants_subject_id_fkey" FOREIGN KEY ("subject_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);

CREATE TABLE "tml_switchboard"."job_grants" (
  "job_id" uuid NOT NULL,
  "subject_id" uuid NOT NULL,
  "permission" "tml_switchboard"."job_permission" NOT NULL,
  "revocable" boolean NOT NULL DEFAULT true,
  "granted_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY ("job_id", "subject_id", "permission"),
  CONSTRAINT "job_grants_job_id_fkey" FOREIGN KEY ("job_id") REFERENCES "tml_switchboard"."jobs" ("job_id") ON UPDATE NO ACTION ON DELETE CASCADE,
  CONSTRAINT "job_grants_subject_id_fkey" FOREIGN KEY ("subject_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);

-- ---------------------------------------------------------------------------
-- Functions and triggers (not managed by Atlas; appended by hand).
-- ---------------------------------------------------------------------------

-- Keep the group membership graph acyclic. See SCHEMA.sql for the full rationale
-- (advisory lock serializes concurrent membership mutations so a cycle-closing
-- edge always sees the committed conflicting edge and aborts).
CREATE OR REPLACE FUNCTION "tml_switchboard"."group_members_no_cycle"()
    RETURNS trigger
    LANGUAGE plpgsql AS
$$
begin
    perform pg_advisory_xact_lock(hashtext('tml_switchboard.group_members'));

    if exists (
        with recursive reach(id) as (
            select NEW.member_id
            union
            select gm.member_id
            from tml_switchboard.group_members gm
            join reach r on gm.group_id = r.id
        )
        select 1 from reach where id = NEW.group_id
    ) then
        raise exception
            'adding % as a member of % would create a cycle', NEW.member_id, NEW.group_id;
    end if;

    return NEW;
end;
$$;

CREATE CONSTRAINT TRIGGER "group_members_acyclic"
    AFTER INSERT OR UPDATE ON "tml_switchboard"."group_members"
    FOR EACH ROW EXECUTE FUNCTION "tml_switchboard"."group_members_no_cycle"();

-- Block deletion or modification of irrevocable (revocable = false) grants.
CREATE OR REPLACE FUNCTION "tml_switchboard"."deny_irrevocable_grant_change"()
    RETURNS trigger
    LANGUAGE plpgsql AS
$$
begin
    if OLD.revocable = false then
        raise exception 'grant on %.% for % is irrevocable',
            TG_TABLE_SCHEMA, TG_TABLE_NAME, OLD.subject_id;
    end if;
    if TG_OP = 'DELETE' then
        return OLD;
    end if;
    return NEW;
end;
$$;

CREATE TRIGGER "supervisor_grants_irrevocable"
    BEFORE DELETE OR UPDATE ON "tml_switchboard"."supervisor_grants"
    FOR EACH ROW EXECUTE FUNCTION "tml_switchboard"."deny_irrevocable_grant_change"();
CREATE TRIGGER "job_grants_irrevocable"
    BEFORE DELETE OR UPDATE ON "tml_switchboard"."job_grants"
    FOR EACH ROW EXECUTE FUNCTION "tml_switchboard"."deny_irrevocable_grant_change"();
