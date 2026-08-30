-- Modify "image_set_members" table
ALTER TABLE "tml_switchboard"."image_set_members"
DROP CONSTRAINT "image_set_members_pkey",
DROP CONSTRAINT "image_set_members_set_id_generation_index_key",
ADD COLUMN "platform_profile" text NULL,
ADD COLUMN "predicate" text NULL,
ADD PRIMARY KEY ("set_id", "generation", "index");
