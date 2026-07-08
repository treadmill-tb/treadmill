-- Rename enum value in "audit_entity_kind" (manually authored; Atlas would
-- only ADD VALUE, leaving the stale 'image_group' value behind)
ALTER TYPE "tml_switchboard"."audit_entity_kind"
RENAME VALUE 'image_group' TO 'image_set';


-- Create enum type "image_set_permission"
CREATE TYPE "tml_switchboard"."image_set_permission" AS ENUM('use', 'manage');


-- Create "image_sets" table
CREATE TABLE "tml_switchboard"."image_sets" (
    "id" uuid NOT NULL,
    "name" text NOT NULL,
    "owner_subject" uuid NULL,
    "label" text NULL,
    "created_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("id"),
    CONSTRAINT "image_sets_name_key" UNIQUE ("name"),
    CONSTRAINT "image_sets_owner_subject_fkey" FOREIGN KEY ("owner_subject") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE SET NULL
);


-- Create "image_set_generations" table
CREATE TABLE "tml_switchboard"."image_set_generations" (
    "set_id" uuid NOT NULL,
    "generation" integer NOT NULL,
    "created_by" uuid NULL,
    "created_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("set_id", "generation"),
    CONSTRAINT "image_set_generations_created_by_fkey" FOREIGN KEY ("created_by") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE SET NULL,
    CONSTRAINT "image_set_generations_set_id_fkey" FOREIGN KEY ("set_id") REFERENCES "tml_switchboard"."image_sets" ("id") ON UPDATE NO ACTION ON DELETE CASCADE,
    CONSTRAINT "generation_positive" CHECK (generation >= 1)
);


-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
DROP CONSTRAINT "valid_init_spec",
ADD CONSTRAINT "valid_init_spec" CHECK (
    (
        (resume_job_id IS NULL)
        AND (
            (
                ((image_id IS NOT NULL))::integer + ((image_set_id IS NOT NULL))::integer
            ) = 1
        )
        AND (
            (image_set_id IS NULL) = (image_set_generation IS NULL)
        )
    )
    OR (
        (resume_job_id IS NOT NULL)
        AND (restart_job_id IS NULL)
        AND (image_id IS NULL)
        AND (image_set_id IS NULL)
        AND (image_set_generation IS NULL)
    )
),
DROP COLUMN "image_group_id",
DROP COLUMN "image_group_generation",
ADD COLUMN "image_set_id" uuid NULL,
ADD COLUMN "image_set_generation" integer NULL,
ADD CONSTRAINT "jobs_image_set_id_image_set_generation_fkey" FOREIGN KEY ("image_set_id", "image_set_generation") REFERENCES "tml_switchboard"."image_set_generations" ("set_id", "generation") ON UPDATE NO ACTION ON DELETE NO ACTION;


-- Create "image_set_grants" table
CREATE TABLE "tml_switchboard"."image_set_grants" (
    "set_id" uuid NOT NULL,
    "subject_id" uuid NOT NULL,
    "permission" "tml_switchboard"."image_set_permission" NOT NULL,
    "granted_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY ("set_id", "subject_id", "permission"),
    CONSTRAINT "image_set_grants_set_id_fkey" FOREIGN KEY ("set_id") REFERENCES "tml_switchboard"."image_sets" ("id") ON UPDATE NO ACTION ON DELETE CASCADE,
    CONSTRAINT "image_set_grants_subject_id_fkey" FOREIGN KEY ("subject_id") REFERENCES "tml_switchboard"."subjects" ("subject_id") ON UPDATE NO ACTION ON DELETE CASCADE
);


-- Create "image_set_members" table
CREATE TABLE "tml_switchboard"."image_set_members" (
    "set_id" uuid NOT NULL,
    "generation" integer NOT NULL,
    "image_id" uuid NOT NULL,
    "required_host_tags" TEXT[] NOT NULL DEFAULT '{}',
    "index" integer NOT NULL,
    PRIMARY KEY ("set_id", "generation", "image_id"),
    CONSTRAINT "image_set_members_set_id_generation_index_key" UNIQUE ("set_id", "generation", "index"),
    CONSTRAINT "image_set_members_image_id_fkey" FOREIGN KEY ("image_id") REFERENCES "tml_switchboard"."images" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION,
    CONSTRAINT "image_set_members_set_id_generation_fkey" FOREIGN KEY ("set_id", "generation") REFERENCES "tml_switchboard"."image_set_generations" ("set_id", "generation") ON UPDATE NO ACTION ON DELETE CASCADE
);


-- Drop "image_group_grants" table
DROP TABLE "tml_switchboard"."image_group_grants";


-- Drop enum type "image_group_permission"
DROP TYPE "tml_switchboard"."image_group_permission";


-- Drop "image_group_members" table
DROP TABLE "tml_switchboard"."image_group_members";


-- Drop "image_group_generations" table
DROP TABLE "tml_switchboard"."image_group_generations";


-- Drop "image_groups" table
DROP TABLE "tml_switchboard"."image_groups";


-- Manually added (community Atlas does not migrate functions/triggers):
-- update the append-only guard's message and re-create the trigger on the
-- renamed generations table (the old trigger was dropped with its table).
CREATE OR REPLACE FUNCTION tml_switchboard.deny_generation_change () returns trigger language plpgsql AS $$
begin
    raise exception 'image-set generations are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;


CREATE TRIGGER image_set_generations_append_only before
UPDATE
OR delete ON tml_switchboard.image_set_generations FOR each ROW
EXECUTE function tml_switchboard.deny_generation_change ();
