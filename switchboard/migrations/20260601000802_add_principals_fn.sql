-- Add the `principals(arg_subject)` authorization helper.
--
-- Atlas's community inspector does not manage Postgres functions, so this object
-- is appended by hand (like the acyclicity trigger and irrevocable-grant guard
-- in the previous migration) and left untouched by future `atlas migrate diff`
-- runs. See SCHEMA.sql for the full rationale: it returns a subject's transitive
-- group memberships (the subject plus every group it reaches via
-- member_id -> group_id edges) and is the shared building block of the ownership
-- and grant checks in src/auth/engine.rs. It is a parameterized function rather
-- than a view so it is seeded with a single subject and walks only that
-- subject's memberships via group_members_member_id_idx.
CREATE OR REPLACE FUNCTION "tml_switchboard"."principals"("arg_subject" uuid)
    RETURNS TABLE ("id" uuid)
    LANGUAGE sql
    STABLE AS
$$
    with recursive reached(id) as (
        select arg_subject
        union
        select gm.group_id
        from tml_switchboard.group_members gm
        join reached r on gm.member_id = r.id
    )
    select id from reached;
$$;
