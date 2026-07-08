import type { components } from "../api/schema";

type JobState = components["schemas"]["JobState"];
type JobInitializingStage = components["schemas"]["JobInitializingStage"];
type TaskExitStatus = components["schemas"]["TaskExitStatus"];
type TerminationReason = components["schemas"]["TerminationReason"];

// Exhaustive switches over the generated unions: a new API variant fails the
// build here instead of rendering wrongly.

export function JobStateBadge({
  state,
  stage,
}: {
  state: JobState;
  stage?: JobInitializingStage | null;
}) {
  let cls: string;
  switch (state) {
    case "queued":
    case "assigned":
      cls = "warn";
      break;
    case "initializing":
    case "ready":
      cls = "active";
      break;
    case "terminating":
      cls = "warn";
      break;
    case "finalized":
      cls = "";
      break;
  }
  const label =
    state === "initializing" && stage != null ? `${state}: ${stage}` : state;
  return <span className={`badge ${cls}`}>{label}</span>;
}

export function TaskExitBadge({
  status,
}: {
  status: TaskExitStatus | null | undefined;
}) {
  if (status == null) {
    return <span className="muted">—</span>;
  }
  let cls: string;
  switch (status) {
    case "pending":
      cls = "active";
      break;
    case "success":
      cls = "ok";
      break;
    case "failure":
      cls = "danger";
      break;
  }
  return <span className={`badge ${cls}`}>{status}</span>;
}

export function TerminationBadge({
  reason,
}: {
  reason: TerminationReason | null | undefined;
}) {
  if (reason == null) {
    return <span className="muted">—</span>;
  }
  let cls: string;
  switch (reason) {
    case "workload_exited":
    case "workload_self_terminated":
    case "user_terminated":
      cls = "";
      break;
    case "queue_timeout":
    case "execution_timeout":
      cls = "warn";
      break;
    case "image_error":
    case "host_match_error":
    case "host_start_failure":
    case "host_dropped_job":
    case "host_unreachable":
    case "resume_failed":
    case "internal_error":
      cls = "danger";
      break;
  }
  return <span className={`badge ${cls}`}>{reason}</span>;
}

export function LiveBadge({ live }: { live: boolean }) {
  return (
    <span className={`badge ${live ? "ok" : "danger"}`}>
      {live ? "live" : "offline"}
    </span>
  );
}
