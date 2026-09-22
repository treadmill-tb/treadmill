import type { components } from "../api/schema";

type JobState = components["schemas"]["JobState"];
type JobInitializingStage = components["schemas"]["JobInitializingStage"];
type TaskExitStatus = components["schemas"]["TaskExitStatus"];
type TerminationReason = components["schemas"]["TerminationReason"];

// Exhaustive records over the generated unions: a new API variant fails the
// build here instead of rendering wrongly.

export type Tone = "ok" | "active" | "warn" | "danger" | "";

const JOB_STATES: Record<JobState, { label: string; tone: Tone }> = {
  queued: { label: "Queued", tone: "warn" },
  assigned: { label: "Assigned", tone: "warn" },
  initializing: { label: "Starting up", tone: "active" },
  ready: { label: "Running", tone: "active" },
  terminating: { label: "Shutting down", tone: "warn" },
  finalized: { label: "Finished", tone: "" },
};

export const INITIALIZING_STAGES: Record<JobInitializingStage, string> = {
  starting: "Starting",
  fetching_image: "Fetching image",
  allocating: "Allocating",
  provisioning: "Provisioning",
  booting: "Booting",
};

const TASK_EXITS: Record<TaskExitStatus, { label: string; tone: Tone }> = {
  pending: { label: "No result yet", tone: "active" },
  success: { label: "Succeeded", tone: "ok" },
  failure: { label: "Failed", tone: "danger" },
};

/** Why a job ended, as a sentence a newcomer understands. */
export const TERMINATION_REASONS: Record<
  TerminationReason,
  { label: string; tone: Tone }
> = {
  workload_exited: { label: "Job terminated normally", tone: "" },
  workload_self_terminated: { label: "Job shut itself down", tone: "" },
  user_terminated: { label: "Terminated by a user", tone: "" },
  execution_timeout: { label: "Stopped when its lease ran out", tone: "warn" },
  preempted: {
    label: "Host reclaimed after the lease ended",
    tone: "warn",
  },
  queue_timeout: { label: "Gave up waiting for a host", tone: "warn" },
  image_error: { label: "Its image could not be used", tone: "danger" },
  host_match_error: { label: "No host matched it", tone: "danger" },
  host_start_failure: { label: "The host failed to start it", tone: "danger" },
  host_dropped_job: { label: "Host dropped job", tone: "danger" },
  host_unreachable: { label: "The host became unreachable", tone: "danger" },
  resume_failed: { label: "Resuming job failed", tone: "danger" },
  internal_error: { label: "Switchboard error", tone: "danger" },
};

export function JobStateBadge({
  state,
  stage,
}: {
  state: JobState;
  stage?: JobInitializingStage | null;
}) {
  const { label, tone } = JOB_STATES[state];
  return (
    <span className={`badge ${tone}`} title={state}>
      {state === "initializing" && stage != null
        ? `${label}: ${INITIALIZING_STAGES[stage].toLowerCase()}`
        : label}
    </span>
  );
}

export function TaskExitBadge({
  status,
}: {
  status: TaskExitStatus | null | undefined;
}) {
  if (status == null) {
    return <span className="muted">—</span>;
  }
  const { label, tone } = TASK_EXITS[status];
  return (
    <span className={`badge ${tone}`} title={status}>
      {label}
    </span>
  );
}

export function TerminationBadge({
  reason,
}: {
  reason: TerminationReason | null | undefined;
}) {
  if (reason == null) {
    return <span className="muted">—</span>;
  }
  const { label, tone } = TERMINATION_REASONS[reason];
  return (
    <span className={`badge ${tone}`} title={reason}>
      {label}
    </span>
  );
}

export function LiveBadge({ live }: { live: boolean }) {
  return (
    <span className={`badge ${live ? "ok" : "danger"}`}>
      {live ? "live" : "offline"}
    </span>
  );
}
