-- Hand-written: Atlas plans these renames as drop-and-add.
ALTER TABLE "tml_switchboard"."image_sets"
RENAME COLUMN "name" TO "canonical_name";


ALTER TABLE "tml_switchboard"."image_sets"
RENAME CONSTRAINT "image_sets_name_key" TO "image_sets_canonical_name_key";


ALTER TABLE "tml_switchboard"."image_sets"
RENAME COLUMN "label" TO "display_name";


UPDATE "tml_switchboard"."image_sets"
SET
    "display_name" = "canonical_name"
WHERE
    "display_name" IS NULL;


ALTER TABLE "tml_switchboard"."image_sets"
ALTER COLUMN "canonical_name"
DROP NOT NULL,
ALTER COLUMN "display_name"
SET NOT NULL;
