-- Factor the host-authorization set out of "job_authorized_hosts" so the
-- enqueue-time and dry-run matching diagnostics, which have no job to key on,
-- count over exactly the set the scheduler will consider.
CREATE FUNCTION tml_switchboard.subject_authorized_hosts (p_subject_id uuid) returns setof uuid language sql stable AS $$
    with owner_principals (id) as (
        select p.id from tml_switchboard.principals(p_subject_id) p
    )
    select h.host_id
    from tml_switchboard.hosts h
    where exists (
              select 1 from owner_principals
              where id = '00000000-0000-0000-0000-000000000001'
          )
       or exists (
              select 1 from owner_principals op where op.id = h.owner_id
          )
       or exists (
              select 1
              from tml_switchboard.host_grants g
              join owner_principals op on g.subject_id = op.id
              where g.host_id = h.host_id and g.permission = 'start'
          );
$$;


-- Same set as before, now delegated. The cross join keeps a nonexistent job
-- yielding no hosts: "principals(NULL)" still carries the "everyone" subject,
-- so a scalar subquery would admit whatever "everyone" holds "start" on.
CREATE OR REPLACE FUNCTION tml_switchboard.job_authorized_hosts (p_job_id uuid) returns setof uuid language sql stable AS $$
    select h.host_id
    from tml_switchboard.jobs j
    cross join lateral tml_switchboard.subject_authorized_hosts(j.owner_id) as h (host_id)
    where j.job_id = p_job_id;
$$;
