-- Rename the managed-device side of the schema from "supervisor" to "host".
--
-- The schema previously conflated two roles on one row in `supervisors`: the
-- managed device (name, tags, ssh_endpoints, owner, current_job) and the
-- credentials/state of the supervisor process driving it (auth_token,
-- worker_instance_id). The host is what jobs run on; the supervisor is the
-- WebSocket peer. Public-facing references now use "host" uniformly; the
-- supervisor name is retained only at the WebSocket protocol layer.
--
-- Authored by hand: Atlas can ADD/RENAME enum values but cannot model a
-- multi-value rename inside `termination_reason` (it interprets the new SCHEMA
-- as a reorder and refuses). `ALTER TYPE … RENAME VALUE` preserves every row
-- that already references the value, which is the correct behavior here.

-- 1. Tables and their key columns.
ALTER TABLE "tml_switchboard"."supervisors" RENAME TO "hosts";
ALTER TABLE "tml_switchboard"."hosts" RENAME COLUMN "supervisor_id" TO "host_id";

ALTER TABLE "tml_switchboard"."supervisor_grants" RENAME TO "host_grants";
ALTER TABLE "tml_switchboard"."host_grants" RENAME COLUMN "supervisor_id" TO "host_id";

-- 2. The dispatch pointer on jobs. CHECK constraint expressions are stored as
--    parsed AST, so the column rename automatically updates the expression in
--    `dispatched_supervisor_iso_assigned`; we still rename the constraint
--    itself for consistency.
ALTER TABLE "tml_switchboard"."jobs" RENAME COLUMN "dispatched_on_supervisor_id" TO "dispatched_on_host_id";
ALTER TABLE "tml_switchboard"."jobs" RENAME CONSTRAINT "dispatched_supervisor_iso_assigned" TO "dispatched_host_iso_assigned";

-- 3. Index, trigger, and the permission type backing host_grants.
ALTER INDEX "tml_switchboard"."supervisors_current_job_unique" RENAME TO "hosts_current_job_unique";
ALTER TRIGGER "supervisor_grants_irrevocable" ON "tml_switchboard"."host_grants" RENAME TO "host_grants_irrevocable";
ALTER TYPE "tml_switchboard"."supervisor_permission" RENAME TO "host_permission";

-- 4. termination_reason enum values that referred to the host (matching, start
--    failure, dropped job, unreachable). The previous values were keyed off the
--    supervisor process; the underlying failure is a host failure from the
--    user's perspective.
ALTER TYPE "tml_switchboard"."termination_reason" RENAME VALUE 'supervisor_match_error'         TO 'host_match_error';
ALTER TYPE "tml_switchboard"."termination_reason" RENAME VALUE 'supervisor_host_start_failure' TO 'host_start_failure';
ALTER TYPE "tml_switchboard"."termination_reason" RENAME VALUE 'supervisor_dropped_job'         TO 'host_dropped_job';
ALTER TYPE "tml_switchboard"."termination_reason" RENAME VALUE 'supervisor_unreachable'         TO 'host_unreachable';

-- 5. Auto-named CHECK/UNIQUE/FOREIGN KEY constraints retain their original
--    `supervisors_*` / `supervisor_grants_*` names through ALTER TABLE RENAME,
--    so rename them explicitly to match the new tables. SCHEMA.sql does not
--    spell these names out (they are inferred from the table name on CREATE),
--    but they must match for `atlas migrate validate` to certify migrations/
--    reproduces SCHEMA.sql.
ALTER TABLE "tml_switchboard"."hosts" RENAME CONSTRAINT "supervisors_auth_token_check"         TO "hosts_auth_token_check";
ALTER TABLE "tml_switchboard"."hosts" RENAME CONSTRAINT "supervisors_worker_instance_id_check" TO "hosts_worker_instance_id_check";
ALTER TABLE "tml_switchboard"."hosts" RENAME CONSTRAINT "supervisors_auth_token_key"           TO "hosts_auth_token_key";
ALTER TABLE "tml_switchboard"."hosts" RENAME CONSTRAINT "supervisors_current_job_fkey"         TO "hosts_current_job_fkey";
ALTER TABLE "tml_switchboard"."hosts" RENAME CONSTRAINT "supervisors_owner_id_fkey"            TO "hosts_owner_id_fkey";

ALTER TABLE "tml_switchboard"."host_grants" RENAME CONSTRAINT "supervisor_grants_subject_id_fkey"    TO "host_grants_subject_id_fkey";
ALTER TABLE "tml_switchboard"."host_grants" RENAME CONSTRAINT "supervisor_grants_supervisor_id_fkey" TO "host_grants_host_id_fkey";
