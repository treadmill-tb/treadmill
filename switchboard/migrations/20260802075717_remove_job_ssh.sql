-- Modify "hosts" table
ALTER TABLE "tml_switchboard"."hosts"
DROP COLUMN "ssh_endpoints";


-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
DROP COLUMN "ssh_keys",
DROP COLUMN "ssh_endpoints";


-- Manually created:
ALTER TYPE tml_switchboard.host_permission
RENAME TO host_permission_old;


CREATE TYPE tml_switchboard.host_permission AS enum('read', 'start', 'manage');


ALTER TABLE tml_switchboard.host_grants
ALTER COLUMN permission TYPE tml_switchboard.host_permission USING permission::text::tml_switchboard.host_permission;


DROP TYPE tml_switchboard.host_permission_old;


ALTER TYPE tml_switchboard.job_permission
RENAME TO job_permission_old;


CREATE TYPE tml_switchboard.job_permission AS enum('read', 'stop', 'manage');


ALTER TABLE tml_switchboard.job_grants
ALTER COLUMN permission TYPE tml_switchboard.job_permission USING permission::text::tml_switchboard.job_permission;


DROP TYPE tml_switchboard.job_permission_old;
