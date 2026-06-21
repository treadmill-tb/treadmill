-- Modify "image_groups" table
ALTER TABLE "tml_switchboard"."image_groups" ADD COLUMN "public" boolean NOT NULL DEFAULT false;
