import {
  CircleAlert,
  CircleCheck,
  CircleDashed,
  CirclePlay,
  CircleStop,
  CircleX,
  Clock,
  Hourglass,
  LoaderCircle,
  type LucideIcon,
} from "lucide-react";
import type { ReactNode } from "react";

import type { components } from "../api/schema";
import { INITIALIZING_STAGES, TERMINATION_REASONS, type Tone } from "./badges";
import { formatSeconds, JobLease } from "./job-lease";
import { RelTime } from "./rel-time";

type JobInfo = components["schemas"]["JobInfo"];
type JobInitializingStage = components["schemas"]["JobInitializingStage"];

const TONE_ICON: Record<Tone, LucideIcon> = {
  ok: CircleCheck,
  active: CircleDashed,
  warn: CircleAlert,
  danger: CircleX,
  "": CircleStop,
};

type HeadlineTone = Tone | "neutral";

function Headline({
  icon: Icon,
  tone,
  children,
}: {
  icon: LucideIcon;
  tone: HeadlineTone;
  children: ReactNode;
}) {
  return (
    <p className={`outcome-headline tone-${tone || "neutral"}`}>
      <Icon size={22} aria-hidden="true" />
      {children}
    </p>
  );
}

/**
 * The job's two independent stories, side by side in every state: where it is
 * in its lifecycle (and what can be done about that), and what its workload
 * has reported.
 */
export function JobStatus({ job }: { job: JobInfo }) {
  return (
    <div className="status-panel outcome">
      <div>
        <h3>Job lifecycle</h3>
        <Lifecycle job={job} />
      </div>
      <div>
        <h3>Workload result</h3>
        <WorkloadResult job={job} />
      </div>
    </div>
  );
}

function Lifecycle({ job }: { job: JobInfo }) {
  const lease = (
    <JobLease job={job} canManage={job.permissions.includes("manage")} />
  );
  switch (job.state) {
    case "queued":
      return (
        <>
          <Headline icon={Clock} tone="warn">
            Waiting for a host
          </Headline>
          <p className="muted">Position in the queue: not available yet.</p>
          {lease}
        </>
      );
    case "assigned":
      return (
        <>
          <Headline icon={Hourglass} tone="warn">
            Assigned to a host
          </Headline>
          <p>Waiting for the host to start the job.</p>
          {lease}
        </>
      );
    case "initializing":
      return (
        <>
          <Headline icon={LoaderCircle} tone="active">
            Starting up
          </Headline>
          <StageProgress stage={job.initializing_stage} />
          {lease}
        </>
      );
    case "ready":
      return (
        <>
          <Headline icon={CirclePlay} tone="active">
            Running
          </Headline>
          {lease}
        </>
      );
    case "terminating":
      return (
        <>
          <Headline icon={CircleStop} tone="warn">
            Shutting down
          </Headline>
          <p>The host is stopping the job.</p>
        </>
      );
    case "finalized":
      return <Ended job={job} />;
  }
}

/** The initialization stages as a bar of steps, the current one animated. */
function StageProgress({
  stage,
}: {
  stage: JobInitializingStage | null | undefined;
}) {
  const stages = Object.entries(INITIALIZING_STAGES);
  const current = stages.findIndex(([s]) => s === stage);
  return (
    <ol className="stage-progress">
      {stages.map(([s, label], i) => (
        <li
          key={s}
          className={i < current ? "done" : i === current ? "current" : ""}
          aria-current={i === current ? "step" : undefined}
        >
          {label}
        </li>
      ))}
    </ol>
  );
}

/** How a finished job ended, and how long it ran. */
function Ended({ job }: { job: JobInfo }) {
  const termination =
    job.termination_reason != null
      ? TERMINATION_REASONS[job.termination_reason]
      : { label: "Ended for an unrecorded reason", tone: "" as const };
  const ran =
    job.started_at != null && job.terminated_at != null
      ? Math.round(
          (Date.parse(job.terminated_at) - Date.parse(job.started_at)) / 1000,
        )
      : null;

  return (
    <>
      <Headline icon={TONE_ICON[termination.tone]} tone={termination.tone}>
        {termination.label}
      </Headline>
      <p className="muted">
        {ran === null
          ? "Never started"
          : `Ran for ${formatSeconds(ran)} of a ${formatSeconds(job.lease_duration_secs)} lease`}
        {job.terminated_at != null && (
          <>
            {" · ended "}
            <RelTime iso={job.terminated_at} />
          </>
        )}
      </p>
    </>
  );
}

/** What the workload itself reported, independent of how the job fares. */
function WorkloadResult({ job }: { job: JobInfo }) {
  const finished = job.state === "finalized";
  let headline: ReactNode;
  switch (job.task_exit_status) {
    case "success":
      headline = (
        <Headline icon={CircleCheck} tone="ok">
          Succeeded
        </Headline>
      );
      break;
    case "failure":
      headline = (
        <Headline icon={CircleX} tone="danger">
          Failed
        </Headline>
      );
      break;
    default:
      headline =
        job.started_at == null && !finished ? (
          <Headline icon={Hourglass} tone="neutral">
            Waiting for the job to start
          </Headline>
        ) : (
          <Headline icon={CircleDashed} tone="neutral">
            {finished ? "No result reported" : "No result reported yet"}
          </Headline>
        );
  }

  return (
    <>
      {headline}
      {job.exit_message != null && <blockquote>{job.exit_message}</blockquote>}
      <p className="muted">
        Report with <code>tml-puppet job result success [&lt;message&gt;]</code>
      </p>
    </>
  );
}
