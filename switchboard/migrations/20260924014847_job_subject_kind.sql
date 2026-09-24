-- Split from the migration that uses it: a new enum value is not usable in the
-- transaction that adds it.
ALTER TYPE "tml_switchboard"."subject_kind"
ADD VALUE 'job';
