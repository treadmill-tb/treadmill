-- Rename "tml_switchboard"."images" column "label" to "title" (the name the
-- OCI manifest annotation uses; the column caches that projection)
ALTER TABLE "tml_switchboard"."images"
RENAME COLUMN "label" TO "title";
