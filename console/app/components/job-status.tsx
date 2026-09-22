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

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { INITIALIZING_STAGES, TERMINATION_REASONS, type Tone } from "./badges";
import { EntityLink } from "./entity-link";
import { HelpTip } from "./help-tip";
import { formatSeconds, JobLease } from "./job-lease";
import { MutationError } from "./mutation-error";
import { RelTime } from "./rel-time";

type JobInfo = components["schemas"]["JobInfo"];
type HostListEntry = components["schemas"]["HostListEntry"];
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
export function JobStatus({
  job,
  hosts,
}: {
  job: JobInfo;
  hosts: HostListEntry[] | undefined;
}) {
  return (
    <div className="status-panel outcome">
      <div>
        <h3>Job lifecycle</h3>
        <Lifecycle job={job} hosts={hosts} />
      </div>
      <div>
        <h3>Workload result</h3>
        <WorkloadResult job={job} />
      </div>
    </div>
  );
}

function Lifecycle({
  job,
  hosts,
}: {
  job: JobInfo;
  hosts: HostListEntry[] | undefined;
}) {
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
          <EligibleHosts job={job} hosts={hosts} />
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

/** The hosts a queued job could be placed on, or why there are none. */
function EligibleHosts({
  job,
  hosts,
}: {
  job: JobInfo;
  hosts: HostListEntry[] | undefined;
}) {
  const report = $api.useQuery("post", "/hosts/match", {
    body: {
      host_cel_predicate: job.host_cel_predicate,
      init_spec:
        job.image.reference.type === "image_set" &&
        job.predecessor?.type !== "resume"
          ? {
              type: "image_set",
              set_id: job.image.reference.set_id,
              generation: job.image.reference.generation,
            }
          : null,
      owner: null,
    },
  });

  if (report.isPending) return <p className="muted">Finding hosts…</p>;
  if (report.isError) {
    return (
      <MutationError
        error={report.error}
        messages={{
          403: "You may not use this job's image set, so its eligible hosts can't be determined.",
        }}
      />
    );
  }
  const r = report.data;
  const predicate = <code>{job.host_cel_predicate}</code>;
  const note = (
    <HelpTip label="About eligible hosts">
      Matched against the hosts <em>you</em> may start jobs on, which can differ
      from the job owner's. Whether a host is currently busy is not taken into
      account.
    </HelpTip>
  );

  if (r.compile_error != null) {
    return (
      <p className="error">
        The host predicate {predicate} doesn't compile: {r.compile_error}
      </p>
    );
  }
  if (r.authorized === 0) {
    return (
      <p className="error">There are no hosts you may start jobs on.{note}</p>
    );
  }
  if (r.schedulable.length === 0) {
    return (
      <>
        <p className="error">
          {r.predicate_matched === 0 ? (
            <>
              None of the {r.authorized} hosts you can use match {predicate}.
            </>
          ) : (
            <>
              {r.predicate_matched} hosts match {predicate}, but none of them
              takes this image set.
            </>
          )}
          {note}
        </p>
        {r.errored > 0 && <PredicateErrors report={r} />}
      </>
    );
  }

  const byName = new Map(hosts?.map((h) => [h.name, h]));
  return (
    <>
      <p>
        {r.schedulable.length} of the {r.authorized} hosts you can use{" "}
        {r.schedulable.length === 1 ? "is" : "are"} eligible:{note}
      </p>
      <ul className="host-chips">
        {r.schedulable.map((name) => {
          const host = byName.get(name);
          return (
            <li key={name}>
              <span
                className={`dot ${host === undefined ? "" : host.maintenance ? "warn" : host.live ? "ok" : "danger"}`}
                title={
                  host === undefined
                    ? undefined
                    : host.maintenance
                      ? "In maintenance"
                      : host.live
                        ? "Live"
                        : "Offline"
                }
              />
              {host === undefined ? (
                name
              ) : (
                <EntityLink kind="host" id={host.host_id} label={name} />
              )}
            </li>
          );
        })}
      </ul>
      {r.errored > 0 && <PredicateErrors report={r} />}
    </>
  );
}

function PredicateErrors({
  report,
}: {
  report: components["schemas"]["HostRequirementsReport"];
}) {
  return (
    <p className="muted">
      The predicate failed to evaluate on {report.errored}{" "}
      {report.errored === 1 ? "host" : "hosts"}, which count as not matching
      {report.errors[0] !== undefined && (
        <>, e.g.: {report.errors[0].message}</>
      )}
      .
    </p>
  );
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
