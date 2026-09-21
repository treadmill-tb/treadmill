-- Functions (atlas community does not diff these).
CREATE OR REPLACE FUNCTION tml_switchboard.job_authorized_hosts (p_job_id uuid) returns setof uuid language sql stable AS $$
    select h.host_id
    from tml_switchboard.jobs j
    cross join lateral tml_switchboard.subject_authorized_hosts(j.owner_id) as h (host_id)
    left join tml_switchboard.jobs pred on pred.job_id = j.resume_job_id
    where j.job_id = p_job_id
      and (
          j.resume_job_id is null
          or h.host_id = pred.dispatched_on_host_id
      );
$$;
