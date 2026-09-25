import { useInfiniteQuery } from "@tanstack/react-query";
import { ChevronRight } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { Link, useSearchParams } from "react-router";

import { $api, client } from "../api/client";
import { ApiError } from "../api/errors";
import { jobImageName } from "../api/images";
import type { components } from "../api/schema";
import {
  lifecycle,
  LifecycleIcon,
  ResultBadge,
  TERMINATION_REASONS,
} from "../components/badges";
import { CopyButton } from "../components/copy-button";
import { EntityLink, ShortId, shortId } from "../components/entity-link";
import { ImageRef } from "../components/image-ref";
import { formatRemaining, formatSeconds } from "../components/job-lease";
import { JobName } from "../components/job-name";
import { RerunButtons } from "../components/job-rerun";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { useDebounced } from "../hooks/use-debounced";
import { useNow } from "../hooks/use-now";

type JobListResponse = components["schemas"]["JobListResponse"];
type JobListState = components["schemas"]["JobListState"];
type JobSummary = components["schemas"]["JobSummary"];
type JobState = components["schemas"]["JobState"];

const SCOPES = [
  { key: "mine", label: "Mine", include: "mine" },
  { key: "groups", label: "+ my groups", include: "mine,groups" },
  {
    key: "shared",
    label: "+ shared with me",
    include: "mine,groups,shared",
  },
  { key: "all", label: "+ all", include: "all" },
  { key: "global", label: "+ global (admin-only)", include: "global" },
] as const;

const ACTIVE_TEXT: Record<Exclude<JobState, "finalized">, string> = {
  queued: "Waiting for a host",
  assigned: "Assigned to a host",
  initializing: "Starting up",
  ready: "Running",
  terminating: "Shutting down",
};

function useJobList(state: JobListState, include: string, q: string) {
  return useInfiniteQuery({
    queryKey: ["jobs", state, include, q],
    queryFn: async ({ pageParam }): Promise<JobListResponse> => {
      const { data, error, response } = await client.GET("/jobs", {
        params: {
          query: {
            include,
            state,
            q: q === "" ? undefined : q,
            cursor: pageParam,
          },
        },
      });
      if (data === undefined) {
        throw new ApiError(response.status, error);
      }
      return data;
    },
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    refetchInterval: 15_000,
  });
}

export default function Jobs() {
  const whoami = $api.useQuery("get", "/auth/whoami");
  const [params, setParams] = useSearchParams();
  const scope = SCOPES.find((s) => s.key === params.get("scope")) ?? SCOPES[0];
  const q = params.get("q") ?? "";
  const [draft, setDraft] = useState(q);
  const debounced = useDebounced(draft.trim(), 300);

  useEffect(() => {
    if (debounced === q) return;
    setParams(
      (p) => {
        if (debounced === "") p.delete("q");
        else p.set("q", debounced);
        return p;
      },
      { replace: true },
    );
  }, [debounced, q, setParams]);

  const active = useJobList("active", scope.include, q);
  const finished = useJobList("finished", scope.include, q);
  const me = whoami.data?.user_id;

  return (
    <>
      <div className="toolbar">
        <h1>Jobs</h1>
        <span className="spacer" />
        <Link to="/jobs/new" className="btn primary">
          Enqueue job
        </Link>
      </div>
      <div className="job-filters">
        <input
          type="search"
          aria-label="Search jobs"
          placeholder="Search name, image, host, owner, ^id"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
        <select
          aria-label="Whose jobs"
          value={scope.key}
          onChange={(e) =>
            setParams(
              (p) => {
                p.set("scope", e.target.value);
                return p;
              },
              { replace: true },
            )
          }
        >
          {SCOPES.filter(
            (s) => s.key !== "global" || whoami.data?.admin === true,
          ).map((s) => (
            <option key={s.key} value={s.key}>
              {s.label}
            </option>
          ))}
        </select>
      </div>

      <JobSection
        title="Active"
        list={active}
        me={me}
        empty={q === "" ? "Nothing is running." : "No active job matches."}
      />
      <JobSection
        title="Finished"
        list={finished}
        me={me}
        byDay
        empty={q === "" ? "No finished jobs." : "No finished job matches."}
      />
    </>
  );
}

function JobSection({
  title,
  list,
  me,
  byDay = false,
  empty,
}: {
  title: string;
  list: ReturnType<typeof useJobList>;
  me: string | undefined;
  byDay?: boolean;
  empty: string;
}) {
  const now = useNow(60_000);
  const jobs = list.data?.pages.flatMap((p) => p.jobs) ?? [];
  const groups: { label: string; jobs: JobSummary[] }[] = [];
  for (const job of jobs) {
    const label = byDay
      ? dayLabel(job.terminated_at ?? job.queued_at, now)
      : title;
    const last = groups.at(-1);
    if (last?.label === label) last.jobs.push(job);
    else groups.push({ label, jobs: [job] });
  }

  return (
    <section className={`job-section${byDay ? "" : " active"}`}>
      <h2>{title}</h2>
      {list.isPending && <p className="muted">Loading…</p>}
      <RequestError
        error={list.error}
        messages={{
          400: "Search terms starting with ^ must be hex digits, and filters such as owner: aren't supported yet.",
        }}
      />
      {list.data && jobs.length === 0 && <p className="muted">{empty}</p>}
      {groups.map((g) => (
        <div key={g.label} className="job-group">
          {byDay && <h3>{g.label}</h3>}
          <div className="job-list">
            {g.jobs.map((job) => (
              <JobRow key={job.job_id} job={job} me={me} now={now} />
            ))}
          </div>
        </div>
      ))}
      {list.hasNextPage && (
        <button
          type="button"
          disabled={list.isFetchingNextPage}
          onClick={() => void list.fetchNextPage()}
        >
          {list.isFetchingNextPage ? "Loading…" : "Show older jobs"}
        </button>
      )}
    </section>
  );
}

function dayLabel(iso: string, now: number): string {
  const date = new Date(iso);
  const day = (d: Date) =>
    new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((day(new Date(now)) - day(date)) / 86_400_000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  return date.toLocaleDateString(undefined, {
    weekday: "long",
    month: "short",
    day: "numeric",
    year:
      date.getFullYear() === new Date(now).getFullYear()
        ? undefined
        : "numeric",
  });
}

function JobRow({
  job,
  me,
  now,
}: {
  job: JobSummary;
  me: string | undefined;
  now: number;
}) {
  const [open, setOpen] = useState(false);
  const life = lifecycle(job);

  return (
    <details
      className="job-row"
      onToggle={(e) => setOpen(e.currentTarget.open)}
    >
      <summary>
        <span title={life.label}>
          <LifecycleIcon life={life} />
        </span>
        <Link to={`/jobs/${job.job_id}`} className="job-row-name">
          <JobName
            label={job.label}
            imageName={jobImageName(job.image.reference, job.image_name)}
            hostId={job.dispatched_on_host_id}
            hostName={job.host_name}
            finished={job.state === "finalized"}
          />
        </Link>
        <span>
          <ResultBadge status={job.task_exit_status} />
        </span>
        <ChevronRight
          size={16}
          className="job-row-chevron"
          aria-hidden="true"
        />
        <span className="job-row-meta">
          <Meta job={job} me={me} now={now} />
        </span>
      </summary>
      {open && <RowDetails job={job} me={me} />}
    </details>
  );
}

function Meta({
  job,
  me,
  now,
}: {
  job: JobSummary;
  me: string | undefined;
  now: number;
}) {
  const parts: ReactNode[] = [];
  const reason =
    job.termination_reason != null
      ? TERMINATION_REASONS[job.termination_reason]
      : null;

  if (job.state !== "finalized") {
    parts.push(
      <span className="life-text-active">{ACTIVE_TEXT[job.state]}</span>,
    );
    if (job.state === "ready" && job.lease_expires_at != null) {
      parts.push(<LeaseLeft job={job} now={now} />);
    }
    parts.push(
      job.started_at != null ? (
        <>
          started <RelTime iso={job.started_at} />
        </>
      ) : (
        <>
          queued <RelTime iso={job.queued_at} />
        </>
      ),
    );
  } else {
    if (reason?.issue) {
      parts.push(<span className="life-text-warn">{reason.label}</span>);
      parts.push(<RelTime iso={job.terminated_at} />);
    } else {
      parts.push(
        <>
          Ended <RelTime iso={job.terminated_at} />
        </>,
      );
    }
    parts.push(ran(job));
  }
  if (job.label != null) {
    parts.push(jobImageName(job.image.reference, job.image_name));
    if (job.dispatched_on_host_id != null) {
      parts.push(job.host_name ?? shortId(job.dispatched_on_host_id));
    }
  }
  parts.push(ownerName(job, me));

  return (
    <>
      {parts.map((p, i) => (
        <span key={i}>{p}</span>
      ))}
    </>
  );
}

function ran(job: JobSummary): string {
  if (job.started_at == null || job.terminated_at == null) {
    return "never started";
  }
  const secs = Math.round(
    (Date.parse(job.terminated_at) - Date.parse(job.started_at)) / 1000,
  );
  return `ran ${formatSeconds(Math.max(secs, 0))}`;
}

function ownerName(job: JobSummary, me: string | undefined): string {
  if (job.owner == null) return "no owner";
  if (job.owner.id === me) return "you";
  const name = job.owner.name ?? shortId(job.owner.id);
  return job.owner.kind === "group" ? `${name} (group)` : name;
}

function LeaseLeft({ job, now }: { job: JobSummary; now: number }) {
  if (job.lease_expires_at == null || job.started_at == null) return null;
  const end = Date.parse(job.lease_expires_at);
  const total = end - Date.parse(job.started_at);
  const left = Math.max(end - now, 0);
  return (
    <span className="job-row-lease">
      <progress value={total > 0 ? left / total : 0} max={1} />
      {formatRemaining(left)} left
    </span>
  );
}

function RowDetails({ job, me }: { job: JobSummary; me: string | undefined }) {
  const info = $api.useQuery("get", "/jobs/{id}", {
    params: { path: { id: job.job_id } },
  });
  const reason =
    job.termination_reason != null
      ? TERMINATION_REASONS[job.termination_reason]
      : null;

  return (
    <div className="job-row-details">
      {info.isPending && <p className="muted">Loading…</p>}
      <RequestError
        error={info.error}
        messages={{ 403: "You no longer have access to this job." }}
      />
      {info.data && (
        <>
          <dl>
            <dt>Job ID</dt>
            <dd>
              <ShortId id={job.job_id} />
              <CopyButton value={job.job_id} label="Copy full job ID" />
            </dd>
            <dt>Image</dt>
            <dd>
              <ImageRef
                image={info.data.image}
                predecessor={info.data.predecessor}
              />
            </dd>
            <dt>Host</dt>
            <dd>
              {job.dispatched_on_host_id == null ? (
                <span className="muted">
                  {job.state === "finalized"
                    ? "never placed"
                    : "to be scheduled"}
                </span>
              ) : (
                <EntityLink
                  kind="host"
                  id={job.dispatched_on_host_id}
                  label={job.host_name ?? undefined}
                />
              )}
            </dd>
            <dt>Owner</dt>
            <dd>{ownerName(job, me)}</dd>
            <dt>Queued</dt>
            <dd>
              <RelTime iso={job.queued_at} />
            </dd>
            {job.started_at != null && (
              <>
                <dt>Started</dt>
                <dd>
                  <RelTime iso={job.started_at} />
                </dd>
              </>
            )}
            {reason != null && (
              <>
                <dt>How it ended</dt>
                <dd>{reason.label}</dd>
              </>
            )}
            {info.data.job_error != null && (
              <>
                <dt>Job error</dt>
                <dd>{info.data.job_error}</dd>
              </>
            )}
            {info.data.exit_message != null && (
              <>
                <dt>Workload said</dt>
                <dd>
                  <q>{info.data.exit_message}</q>
                </dd>
              </>
            )}
          </dl>
          <div className="job-row-actions">
            <Link className="btn primary" to={`/jobs/${job.job_id}`}>
              Open job <ChevronRight size={14} aria-hidden="true" />
            </Link>
            {job.state === "finalized" &&
              info.data.permissions.includes("manage") && (
                <RerunButtons job={info.data} />
              )}
          </div>
        </>
      )}
    </div>
  );
}
