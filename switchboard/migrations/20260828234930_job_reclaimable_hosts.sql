-- Functions (atlas community does not diff these).
CREATE FUNCTION tml_switchboard.reclaimable_hosts (
    p_job_id uuid,
    p_liveness_cutoff timestamp with time zone,
    p_now timestamp with time zone
) returns TABLE (host_id uuid, reclaim_pending boolean) language sql stable AS $$
    select h.host_id, v.terminate_requested_at is not null
    from tml_switchboard.hosts h
    join tml_switchboard.jobs v on v.job_id = h.current_job
    join tml_switchboard.job_authorized_hosts(p_job_id) a on a = h.host_id
    where h.last_seen_at is not null
      and h.last_seen_at > p_liveness_cutoff
      and h.tags @> (
          select host_tag_requirements
          from tml_switchboard.jobs
          where job_id = p_job_id
      )
      and v.job_state <> 'finalized'
      and v.lease_expiry_action = 'preempt'
      and v.started_at is not null
      and v.started_at + v.lease_duration <= p_now
    order by v.started_at + v.lease_duration;
$$;
