-- Create index "hosts_tags_gin" to table: "hosts"
CREATE INDEX "hosts_tags_gin" ON "tml_switchboard"."hosts" USING gin ("tags");
-- Modify "image_group_members" table
ALTER TABLE "tml_switchboard"."image_group_members" DROP COLUMN "arch", DROP COLUMN "os", DROP COLUMN "variant", DROP COLUMN "tml_target", DROP COLUMN "tml_board", ADD COLUMN "required_host_tags" text[] NOT NULL DEFAULT '{}', ADD COLUMN "position" integer NOT NULL;
-- Create "host_targets" table
CREATE TABLE "tml_switchboard"."host_targets" ("target_id" uuid NOT NULL, "host_id" uuid NOT NULL, "name" text NOT NULL, "tags" text[] NOT NULL DEFAULT '{}', PRIMARY KEY ("target_id"), CONSTRAINT "host_targets_host_id_name_key" UNIQUE ("host_id", "name"), CONSTRAINT "host_targets_host_id_fkey" FOREIGN KEY ("host_id") REFERENCES "tml_switchboard"."hosts" ("host_id") ON UPDATE NO ACTION ON DELETE CASCADE);
-- Create index "host_targets_tags_gin" to table: "host_targets"
CREATE INDEX "host_targets_tags_gin" ON "tml_switchboard"."host_targets" USING gin ("tags");
-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs" DROP COLUMN "tag_config", ADD COLUMN "host_tag_requirements" text[] NOT NULL DEFAULT '{}';
-- Create "job_target_requirements" table
CREATE TABLE "tml_switchboard"."job_target_requirements" ("job_id" uuid NOT NULL, "req_index" integer NOT NULL, "tags" text[] NOT NULL DEFAULT '{}', PRIMARY KEY ("job_id", "req_index"), CONSTRAINT "job_target_requirements_job_id_fkey" FOREIGN KEY ("job_id") REFERENCES "tml_switchboard"."jobs" ("job_id") ON UPDATE NO ACTION ON DELETE CASCADE);
