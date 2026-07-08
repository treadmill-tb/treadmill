INSERT INTO
    tml_switchboard.subjects (subject_id, kind)
VALUES
    ('00000000-0000-0000-0000-000000000004', 'group');


INSERT INTO
    tml_switchboard.groups (subject_id, name)
VALUES
    (
        '00000000-0000-0000-0000-000000000004',
        'everyone'
    );


-- Preserve existing public groups: a public group becomes one that grants the
-- `everyone` subject `use`. Must run before the `public` column is dropped.
INSERT INTO
    tml_switchboard.image_group_grants (group_id, subject_id, permission)
SELECT
    id,
    '00000000-0000-0000-0000-000000000004'::uuid,
    'use'::tml_switchboard.image_group_permission
FROM
    tml_switchboard.image_groups
WHERE
    public
ON CONFLICT DO NOTHING;


CREATE OR REPLACE FUNCTION tml_switchboard.principals (arg_subject uuid) returns TABLE (id uuid) language sql stable AS $$
    with recursive reached(id) as (
        select arg_subject
        union
        select gm.group_id
        from tml_switchboard.group_members gm
        join reached r on gm.member_id = r.id
    )
    select id from reached
    union
    select '00000000-0000-0000-0000-000000000004'::uuid;
$$;


-- Modify "image_groups" table
ALTER TABLE "tml_switchboard"."image_groups"
DROP COLUMN "public";
