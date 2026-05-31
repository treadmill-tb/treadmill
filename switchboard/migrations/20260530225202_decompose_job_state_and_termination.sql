-- Create enum type "job_state"
CREATE TYPE "tml_switchboard"."job_state" AS ENUM ('queued', 'scheduled', 'initializing', 'ready', 'terminating', 'finalized');
-- Create enum type "job_initializing_stage"
CREATE TYPE "tml_switchboard"."job_initializing_stage" AS ENUM ('starting', 'fetching_image', 'allocating', 'provisioning', 'booting');
-- Create enum type "termination_reason"
CREATE TYPE "tml_switchboard"."termination_reason" AS ENUM ('workload_exited', 'workload_self_canceled', 'user_canceled', 'queue_timeout', 'execution_timeout', 'image_error', 'supervisor_match_error', 'supervisor_host_start_failure', 'supervisor_dropped_job', 'supervisor_unreachable', 'resume_failed', 'internal_error');
-- Create enum type "task_exit_status"
CREATE TYPE "tml_switchboard"."task_exit_status" AS ENUM ('pending', 'success', 'failure');
-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs" DROP CONSTRAINT "exit_status_allows_host_output", DROP CONSTRAINT "exit_status_nullity_iso_finalized", DROP CONSTRAINT "valid_dispatched_implication", DROP CONSTRAINT "valid_queued_implication", ADD CONSTRAINT "dispatched_supervisor_iso_assigned" CHECK ((dispatched_on_supervisor_id IS NOT NULL) = (job_state = ANY (ARRAY['scheduled'::tml_switchboard.job_state, 'initializing'::tml_switchboard.job_state, 'ready'::tml_switchboard.job_state, 'terminating'::tml_switchboard.job_state]))), ADD CONSTRAINT "initializing_stage_iso_initializing" CHECK ((initializing_stage IS NOT NULL) = (job_state = 'initializing'::tml_switchboard.job_state)), ADD CONSTRAINT "started_at_iso_executing" CHECK ((started_at IS NOT NULL) = (job_state = ANY (ARRAY['initializing'::tml_switchboard.job_state, 'ready'::tml_switchboard.job_state, 'terminating'::tml_switchboard.job_state]))), ADD CONSTRAINT "task_exit_status_absent_while_queued" CHECK ((task_exit_status IS NULL) OR (job_state <> 'queued'::tml_switchboard.job_state)), ADD CONSTRAINT "terminated_at_iso_finalized" CHECK ((terminated_at IS NOT NULL) = (job_state = 'finalized'::tml_switchboard.job_state)), ADD CONSTRAINT "termination_reason_iso_finalized" CHECK ((termination_reason IS NOT NULL) = (job_state = 'finalized'::tml_switchboard.job_state)), DROP COLUMN "functional_state", DROP COLUMN "exit_status", DROP COLUMN "host_output", ADD COLUMN "job_state" "tml_switchboard"."job_state" NOT NULL, ADD COLUMN "initializing_stage" "tml_switchboard"."job_initializing_stage" NULL, ADD COLUMN "termination_reason" "tml_switchboard"."termination_reason" NULL, ADD COLUMN "task_exit_status" "tml_switchboard"."task_exit_status" NULL, ADD COLUMN "exit_message" text NULL;
-- Drop enum type "functional_state"
DROP TYPE "tml_switchboard"."functional_state";
-- Drop enum type "exit_status"
DROP TYPE "tml_switchboard"."exit_status";
