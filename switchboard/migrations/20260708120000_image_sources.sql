CREATE TYPE tml_switchboard.image_source_permission AS enum('use', 'manage');


CREATE TABLE tml_switchboard.image_sources (
    id uuid NOT NULL PRIMARY KEY,
    image_id uuid NOT NULL REFERENCES tml_switchboard.images (id) ON DELETE CASCADE,
    registry text NOT NULL,
    repository text NOT NULL,
    status text NOT NULL,
    owner_subject uuid REFERENCES tml_switchboard.subjects (subject_id) ON DELETE SET NULL,
    added_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    UNIQUE (image_id, registry, repository),
    CONSTRAINT valid_location_status CHECK (status IN ('external', 'canonical', 'system'))
);


CREATE TABLE tml_switchboard.image_source_grants (
    source_id uuid NOT NULL REFERENCES tml_switchboard.image_sources (id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES tml_switchboard.subjects (subject_id) ON DELETE CASCADE,
    permission tml_switchboard.image_source_permission NOT NULL,
    granted_at timestamp with time zone NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (source_id, subject_id, permission)
);


-- Usability helper (hand-carried; Atlas won't diff function bodies).
CREATE FUNCTION tml_switchboard.image_source_usable (p_subject uuid, p_image uuid) returns boolean language sql stable AS $$
    select exists (
        select 1
        from tml_switchboard.image_sources s
        where s.image_id = p_image
          and (
              exists (
                  select 1 from tml_switchboard.principals(p_subject) pr
                  where pr.id = '00000000-0000-0000-0000-000000000001'::uuid
              )
              or exists (
                  select 1 from tml_switchboard.principals(p_subject) pr
                  where pr.id = s.owner_subject
              )
              or exists (
                  select 1
                  from tml_switchboard.image_source_grants g
                  join tml_switchboard.principals(p_subject) pr on g.subject_id = pr.id
                  where g.source_id = s.id and g.permission = 'use'
              )
          )
    );
$$;


-- Copy existing locations into sources: mint an id and inherit the (soon-dropped)
-- image owner as the source owner.
INSERT INTO
    tml_switchboard.image_sources (
        id,
        image_id,
        registry,
        repository,
        status,
        owner_subject,
        added_at
    )
SELECT
    gen_random_uuid(),
    l.image_id,
    l.registry,
    l.repository,
    l.status,
    i.owner_subject,
    l.added_at
FROM
    tml_switchboard.image_locations l
    JOIN tml_switchboard.images i ON i.id = l.image_id;


-- Preserve "any image usable by anyone": grant `everyone` `use` on every source.
INSERT INTO
    tml_switchboard.image_source_grants (source_id, subject_id, permission)
SELECT
    s.id,
    '00000000-0000-0000-0000-000000000004'::uuid,
    'use'::tml_switchboard.image_source_permission
FROM
    tml_switchboard.image_sources s
ON CONFLICT DO NOTHING;


DROP TABLE tml_switchboard.image_locations;


ALTER TABLE tml_switchboard.images
DROP COLUMN owner_subject;
