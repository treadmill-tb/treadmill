-- Replace the OCI-index image-group model with mutable, named, generationed
-- groups (see doc/image-groups-mutable-generations-plan.md). Destructive: the
-- old `index_digest` group / denormalized-member shape is dropped (no group data
-- to preserve pre-production). The append-only trigger at the bottom is
-- hand-appended: Atlas's community inspector does not manage Postgres functions
-- or triggers (see 20260601015052_add_audit_log.sql), so it lives outside the
-- diff and is left untouched by future `atlas migrate diff` runs.

-- Create enum type "image_group_permission"
CREATE TYPE "tml_switchboard"."image_group_permission" AS ENUM ('use', 'manage');
-- Modify "image_groups" table
ALTER TABLE "tml_switchboard"."image_groups" DROP COLUMN "index_digest", ADD COLUMN "name" text NOT NULL, ADD CONSTRAINT "image_groups_name_key" UNIQUE ("name");
-- Create "image_group_generations" table
CREATE TABLE "tml_switchboard"."image_group_generations" ("group_id" uuid NOT NULL, "generation" integer NOT NULL, "created_by" uuid NULL, "created_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY ("group_id", "generation"), CONSTRAINT "image_group_generations_created_by_fkey" FOREIGN KEY ("created_by") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE SET NULL, CONSTRAINT "image_group_generations_group_id_fkey" FOREIGN KEY ("group_id") REFERENCES "tml_switchboard"."image_groups" ("id") ON UPDATE NO ACTION ON DELETE CASCADE, CONSTRAINT "generation_positive" CHECK (generation >= 1));
-- Create "image_group_grants" table
CREATE TABLE "tml_switchboard"."image_group_grants" ("group_id" uuid NOT NULL, "subject_id" uuid NOT NULL, "permission" "tml_switchboard"."image_group_permission" NOT NULL, "granted_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY ("group_id", "subject_id", "permission"), CONSTRAINT "image_group_grants_group_id_fkey" FOREIGN KEY ("group_id") REFERENCES "tml_switchboard"."image_groups" ("id") ON UPDATE NO ACTION ON DELETE CASCADE, CONSTRAINT "image_group_grants_subject_id_fkey" FOREIGN KEY ("subject_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE);
-- Modify "image_group_members" table
ALTER TABLE "tml_switchboard"."image_group_members" DROP CONSTRAINT "image_group_members_pkey", DROP CONSTRAINT "image_group_members_group_id_fkey", DROP CONSTRAINT "image_group_members_image_id_fkey", DROP COLUMN "position", ADD COLUMN "generation" integer NOT NULL, ADD COLUMN "index" integer NOT NULL, ADD PRIMARY KEY ("group_id", "generation", "image_id"), ADD CONSTRAINT "image_group_members_group_id_generation_index_key" UNIQUE ("group_id", "generation", "index"), ADD CONSTRAINT "image_group_members_image_id_fkey" FOREIGN KEY ("image_id") REFERENCES "tml_switchboard"."images" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION, ADD CONSTRAINT "image_group_members_group_id_generation_fkey" FOREIGN KEY ("group_id", "generation") REFERENCES "tml_switchboard"."image_group_generations" ("group_id", "generation") ON UPDATE NO ACTION ON DELETE CASCADE;
-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs" DROP CONSTRAINT "valid_init_spec", ADD CONSTRAINT "valid_init_spec" CHECK (((resume_job_id IS NULL) AND ((((image_id IS NOT NULL))::integer + ((image_group_id IS NOT NULL))::integer) = 1) AND ((image_group_id IS NULL) = (image_group_generation IS NULL))) OR ((resume_job_id IS NOT NULL) AND (restart_job_id IS NULL) AND (image_id IS NULL) AND (image_group_id IS NULL) AND (image_group_generation IS NULL))), DROP COLUMN "image_digest", DROP COLUMN "image_group_digest", DROP COLUMN "resolved_image_digest", ADD COLUMN "image_id" uuid NULL, ADD COLUMN "image_group_id" uuid NULL, ADD COLUMN "image_group_generation" integer NULL, ADD COLUMN "resolved_image_id" uuid NULL, ADD CONSTRAINT "jobs_image_group_id_image_group_generation_fkey" FOREIGN KEY ("image_group_id", "image_group_generation") REFERENCES "tml_switchboard"."image_group_generations" ("group_id", "generation") ON UPDATE NO ACTION ON DELETE NO ACTION, ADD CONSTRAINT "jobs_image_id_fkey" FOREIGN KEY ("image_id") REFERENCES "tml_switchboard"."images" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION, ADD CONSTRAINT "jobs_resolved_image_id_fkey" FOREIGN KEY ("resolved_image_id") REFERENCES "tml_switchboard"."images" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION;

-- Enforce append-only on `image_group_generations`: a generation snapshot is
-- immutable once created (every membership edit appends a new generation). Same
-- pattern as `deny_audit_event_change`; hand-appended (Atlas ignores it).
CREATE OR REPLACE FUNCTION "tml_switchboard"."deny_generation_change"()
    RETURNS trigger
    LANGUAGE plpgsql AS
$$
begin
    raise exception 'image-group generations are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;

CREATE TRIGGER "image_group_generations_append_only"
    BEFORE DELETE OR UPDATE ON "tml_switchboard"."image_group_generations"
    FOR EACH ROW EXECUTE FUNCTION "tml_switchboard"."deny_generation_change"();
