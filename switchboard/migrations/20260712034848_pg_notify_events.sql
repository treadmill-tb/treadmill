-- Row-change wake-ups over LISTEN/NOTIFY on channel `tml_events`; see the
-- CHANGE NOTIFICATIONS section of SCHEMA.sql for the payload contract.
CREATE OR REPLACE FUNCTION tml_switchboard.notify_change () returns trigger language plpgsql AS $$
declare
    guc text := 'tml_switchboard.notify_rows_' || TG_TABLE_NAME;
    n int := coalesce(nullif(current_setting(guc, true), ''), '0')::int + 1;
    row_limit int := coalesce(
        nullif(current_setting('tml_switchboard.notify_row_limit', true), ''),
        '100')::int;
    old_j jsonb;
    new_j jsonb;
    ks jsonb := '{}'::jsonb;
    col text;
    vals jsonb;
begin
    perform set_config(guc, n::text, true);
    if n > row_limit then
        perform pg_notify('tml_events',
            jsonb_build_object('table', TG_TABLE_NAME)::text);
        return null;
    end if;
    old_j := case when TG_OP <> 'INSERT' then to_jsonb(OLD) end;
    new_j := case when TG_OP <> 'DELETE' then to_jsonb(NEW) end;
    foreach col in array TG_ARGV loop
        select coalesce(jsonb_agg(distinct v), '[]'::jsonb) into vals
        from unnest(array[old_j -> col, new_j -> col]) as t (v)
        where v is not null and v <> 'null'::jsonb;
        if vals <> '[]'::jsonb then
            ks := ks || jsonb_build_object(col, vals);
        end if;
    end loop;
    perform pg_notify('tml_events',
        jsonb_build_object('table', TG_TABLE_NAME, 'keys', ks)::text);
    return null;
end;
$$;


CREATE TRIGGER jobs_notify_write
AFTER insert
OR delete ON tml_switchboard.jobs FOR each ROW
EXECUTE function tml_switchboard.notify_change ('job_id', 'dispatched_on_host_id');


CREATE TRIGGER jobs_notify_update
AFTER
UPDATE ON tml_switchboard.jobs FOR each ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)
EXECUTE function tml_switchboard.notify_change ('job_id', 'dispatched_on_host_id');


CREATE TRIGGER hosts_notify_write
AFTER insert
OR delete ON tml_switchboard.hosts FOR each ROW
EXECUTE function tml_switchboard.notify_change ('host_id');


CREATE TRIGGER hosts_notify_update
AFTER
UPDATE OF current_job,
tags,
owner_id,
worker_instance_id ON tml_switchboard.hosts FOR each ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)
EXECUTE function tml_switchboard.notify_change ('host_id');
