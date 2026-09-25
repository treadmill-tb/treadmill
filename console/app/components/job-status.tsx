import { CircleDashed } from "lucide-react";
import type { ReactNode } from "react";

import type { components } from "../api/schema";
import {
  INITIALIZING_STAGES,
  lifecycle,
  LifecycleIcon,
  ResultBadge,
  TERMINATION_REASONS,
} from "./badges";
import { formatSeconds, JobLease } from "./job-lease";
import { RelTime } from "./rel-time";

type JobInfo = components["schemas"]["JobInfo"];
type JobInitializingStage = components["schemas"]["JobInitializingStage"];

function Headline({ job, children }: { job: JobInfo; children: ReactNode }) {
  const life = lifecycle(job);
  return (
    <p className={`outcome-headline tone-${life.tone}`}>
      <LifecycleIcon life={life} size={22} />
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
          <Headline job={job}>Waiting for a host</Headline>
          <p className="muted">Position in the queue: not available yet.</p>
          {lease}
        </>
      );
    case "assigned":
      return (
        <>
          <Headline job={job}>Assigned to a host</Headline>
          <p>Waiting for the host to start the job.</p>
          {lease}
        </>
      );
    case "initializing":
      return (
        <>
          <Headline job={job}>Starting up</Headline>
          <StageProgress stage={job.initializing_stage} />
          {lease}
        </>
      );
    case "ready":
      return (
        <>
          <Headline job={job}>Running</Headline>
          {lease}
        </>
      );
    case "terminating":
      return (
        <>
          <Headline job={job}>Shutting down</Headline>
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
      : { label: "Ended for an unrecorded reason", issue: false };
  const ran =
    job.started_at != null && job.terminated_at != null
      ? Math.round(
          (Date.parse(job.terminated_at) - Date.parse(job.started_at)) / 1000,
        )
      : null;

  return (
    <>
      <Headline job={job}>
        {termination.issue
          ? `Job error: ${termination.label.charAt(0).toLowerCase()}${termination.label.slice(1)}`
          : termination.label}
      </Headline>
      {job.job_error != null && <blockquote>{job.job_error}</blockquote>}
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
  const reported =
    job.task_exit_status === "success" || job.task_exit_status === "failure";

  return (
    <>
      {reported ? (
        <ResultBadge
          status={job.task_exit_status}
          size={22}
          className="outcome-headline result-block"
        />
      ) : (
        <p className="outcome-headline tone-neutral">
          <CircleDashed size={22} className="life-neutral" aria-hidden="true" />
          {job.started_at == null && !finished
            ? "Waiting for the job to start"
            : finished
              ? "No result reported"
              : "No result reported yet"}
        </p>
      )}
      {job.exit_message != null && <blockquote>{job.exit_message}</blockquote>}
      <p className="muted">
        Report with{" "}
        <code>tml job set-exit-status success|failure [&lt;message&gt;]</code>
      </p>
    </>
  );
}
