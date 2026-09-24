-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
DROP CONSTRAINT "terminate_requested_reason_valid",
ADD CONSTRAINT "terminate_requested_reason_valid" CHECK (
    terminate_requested_reason = ANY (
        ARRAY[
            'user_terminated'::tml_switchboard.termination_reason,
            'workload_self_terminated'::tml_switchboard.termination_reason,
            'preempted'::tml_switchboard.termination_reason
        ]
    )
);
