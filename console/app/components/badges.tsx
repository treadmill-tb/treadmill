import {
  CircleCheck,
  CirclePlay,
  CircleStop,
  CircleX,
  Clock,
  Flag,
  Hourglass,
  LoaderCircle,
  type LucideIcon,
  TriangleAlert,
} from "lucide-react";

import type { components } from "../api/schema";

type JobState = components["schemas"]["JobState"];
type JobInitializingStage = components["schemas"]["JobInitializingStage"];
type TaskExitStatus = components["schemas"]["TaskExitStatus"];
type TerminationReason = components["schemas"]["TerminationReason"];

// Exhaustive records over the generated unions: a new API variant fails the
// build here instead of rendering wrongly.

export type Tone = "ok" | "active" | "warn" | "danger" | "";

export type LifecycleTone = "neutral" | "active" | "warn";

export interface Lifecycle {
  label: string;
  icon: LucideIcon;
  tone: LifecycleTone;
  spin?: boolean;
}

const LIFECYCLE: Record<JobState, Lifecycle> = {
  queued: { label: "Queued", icon: Clock, tone: "neutral" },
  assigned: { label: "Assigned", icon: Hourglass, tone: "neutral" },
  initializing: {
    label: "Starting up",
    icon: LoaderCircle,
    tone: "active",
    spin: true,
  },
  ready: { label: "Running", icon: CirclePlay, tone: "active" },
  terminating: { label: "Shutting down", icon: CircleStop, tone: "active" },
  finalized: { label: "Finished", icon: Flag, tone: "neutral" },
};

const JOB_ERROR: Lifecycle = {
  label: "Job error",
  icon: TriangleAlert,
  tone: "warn",
};

export function lifecycle(job: {
  state: JobState;
  termination_reason?: TerminationReason | null;
}): Lifecycle {
  return job.state === "finalized" &&
    job.termination_reason != null &&
    TERMINATION_REASONS[job.termination_reason].issue
    ? JOB_ERROR
    : LIFECYCLE[job.state];
}

export function LifecycleIcon({
  life,
  size = 16,
}: {
  life: Lifecycle;
  size?: number;
}) {
  const Icon = life.icon;
  return (
    <Icon
      size={size}
      className={`life-icon life-${life.tone}${life.spin ? " spin" : ""}`}
      aria-hidden="true"
    />
  );
}

export const INITIALIZING_STAGES: Record<JobInitializingStage, string> = {
  starting: "Starting",
  fetching_image: "Fetching image",
  allocating: "Allocating",
  provisioning: "Provisioning",
  booting: "Booting",
};

/** Why a job ended, as a sentence a newcomer understands. */
export const TERMINATION_REASONS: Record<
  TerminationReason,
  { label: string; issue: boolean }
> = {
  workload_exited: { label: "Job terminated normally", issue: false },
  workload_self_terminated: { label: "Job shut itself down", issue: false },
  user_terminated: { label: "Terminated by a user", issue: false },
  execution_timeout: { label: "Stopped when its lease ran out", issue: true },
  preempted: { label: "Host reclaimed after the lease ended", issue: true },
  queue_timeout: { label: "Gave up waiting for a host", issue: true },
  image_error: { label: "The image could not be used", issue: true },
  host_match_error: { label: "No host matched the job", issue: true },
  host_start_failure: {
    label: "The host failed to start the job",
    issue: true,
  },
  host_dropped_job: { label: "The host dropped the job", issue: true },
  host_unreachable: { label: "The host became unreachable", issue: true },
  resume_failed: { label: "Resuming the job failed", issue: true },
  internal_error: { label: "Internal error on the host", issue: true },
};

export function JobStateBadge({
  job,
}: {
  job: {
    state: JobState;
    initializing_stage?: JobInitializingStage | null;
    termination_reason?: TerminationReason | null;
  };
}) {
  const life = lifecycle(job);
  return (
    <span className={`badge state-${life.tone}`} title={job.state}>
      <LifecycleIcon life={life} size={14} />
      {job.state === "initializing" && job.initializing_stage != null
        ? `${life.label}: ${INITIALIZING_STAGES[job.initializing_stage].toLowerCase()}`
        : life.label}
    </span>
  );
}

const RESULTS: Record<
  Exclude<TaskExitStatus, "pending">,
  { label: string; icon: LucideIcon; tone: "ok" | "danger" }
> = {
  success: { label: "Succeeded", icon: CircleCheck, tone: "ok" },
  failure: { label: "Failed", icon: CircleX, tone: "danger" },
};

export function ResultBadge({
  status,
  size = 14,
  className = "badge",
}: {
  status: TaskExitStatus | null | undefined;
  size?: number;
  className?: string;
}) {
  if (status == null || status === "pending") return null;
  const { label, icon: Icon, tone } = RESULTS[status];
  return (
    <span className={`${className} result-${tone}`} title={status}>
      <Icon size={size} className="filled" aria-hidden="true" />
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
  const { label, issue } = TERMINATION_REASONS[reason];
  return (
    <span className={`badge ${issue ? "warn" : ""}`} title={reason}>
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
