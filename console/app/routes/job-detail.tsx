import { useQueryClient } from "@tanstack/react-query";
import { Check, Pencil, Server, User, X } from "lucide-react";
import { useState } from "react";
import { Link, useSearchParams } from "react-router";

import { $api } from "../api/client";
import { ApiError, describeError } from "../api/errors";
import type { components } from "../api/schema";
import { AuditLog } from "../components/audit-log";
import { JobStateBadge } from "../components/badges";
import { CopyButton } from "../components/copy-button";
import { ConfirmDialog } from "../components/dialog";
import { EntityLink, ShortId } from "../components/entity-link";
import { HostCard } from "../components/host-card";
import { JobDetails } from "../components/job-details";
import { JobLog, parseReplayBytes } from "../components/job-log";
import { JobServices } from "../components/job-services";
import { JobStatus } from "../components/job-status";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { useResourceWatch } from "../hooks/use-resource-watch";
import { useUpdateJob } from "../hooks/use-update-job";
import type { Route } from "./+types/job-detail";

type JobInfo = components["schemas"]["JobInfo"];

export default function JobDetail({ params }: Route.ComponentProps) {
  const queryClient = useQueryClient();
  const watchStopped = useResourceWatch(`/jobs/${params.id}/watch`, [
    "get",
    "/jobs/{id}",
  ]);
  // Per-page-load override for how much log history to replay (a user
  // settings page may subsume this later).
  const [searchParams] = useSearchParams();
  const replayBytes = parseReplayBytes(searchParams.get("replay"));
  const job = $api.useQuery("get", "/jobs/{id}", {
    params: { path: { id: params.id } },
  });
  const hosts = $api.useQuery("get", "/hosts");
  const terminate = $api.useMutation("delete", "/jobs/{id}", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["get", "/jobs/{id}"] }),
        queryClient.invalidateQueries({ queryKey: ["jobs"] }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "jobs", params.id],
        }),
      ]);
    },
  });
  const [confirmTerminate, setConfirmTerminate] = useState(false);

  if (job.isPending) return <p className="muted">Loading…</p>;
  if (job.isError) {
    return (
      <RequestError
        error={job.error}
        messages={{
          403: "This job doesn't exist, or you don't have access to it.",
        }}
      />
    );
  }

  const data = job.data;
  const host = hosts.data?.find(
    (h) => h.host_id === data.dispatched_on_host_id,
  );
  const finalized = data.state === "finalized";
  const canManage = data.permissions.includes("manage");

  return (
    <>
      <header className="page-head">
        <div className="page-head-title">
          <JobName job={data} />
          <span className="page-id">
            (
            <ShortId id={data.job_id} />
            <CopyButton value={data.job_id} label="Copy full job ID" />)
          </span>
        </div>
        <div className="page-head-actions">
          <JobStateBadge state={data.state} stage={data.initializing_stage} />
          {finalized ? (
            <Link className="btn" to="/jobs/new">
              Resume
            </Link>
          ) : (
            data.permissions.includes("stop") && (
              <button
                type="button"
                className="danger"
                disabled={data.state === "terminating" || terminate.isPending}
                onClick={() => setConfirmTerminate(true)}
              >
                {terminate.isPending ? "Terminating…" : "Terminate"}
              </button>
            )
          )}
        </div>
      </header>
      <JobContext job={data} hostName={host?.name} />
      {watchStopped !== null && (
        <p className="error">
          Live updates stopped.{" "}
          {describeError(new ApiError(watchStopped, undefined), {
            403: "You no longer have access to this job.",
          })}{" "}
          Reload the page to see its latest state.
        </p>
      )}
      <RequestError
        error={terminate.error}
        messages={{ 403: "You are not allowed to terminate this job." }}
      />
      <ConfirmDialog
        open={confirmTerminate}
        title="Terminate this job?"
        confirmLabel="Terminate"
        danger
        onConfirm={() => {
          setConfirmTerminate(false);
          terminate.mutate({ params: { path: { id: params.id } } });
        }}
        onCancel={() => setConfirmTerminate(false)}
      >
        <p>
          {data.label != null ? <strong>{data.label}</strong> : "This job"} (
          <ShortId id={data.job_id} />) will be stopped
          {data.dispatched_on_host_id != null && " and its host freed up"}. This
          cannot be undone.
        </p>
      </ConfirmDialog>

      <JobStatus job={data} hosts={hosts.data} />

      {!finalized && (
        <JobServices
          jobId={params.id}
          services={data.services}
          canOpen={canManage && !finalized}
        />
      )}

      <div className="job-cards">
        <JobDetails job={data} />
        {host !== undefined && <HostCard host={host} />}
      </div>

      <JobLog
        key={`${params.id} ${replayBytes}`}
        jobId={params.id}
        dispatched={data.dispatched_on_host_id != null}
        replayBytes={replayBytes}
        finalized={finalized}
        canSendInput={canManage && !finalized}
      />

      <AuditLog entity="jobs" id={params.id} />
    </>
  );
}

/** The job's name as the page title; the pencil turns it into a text box. */
function JobName({ job }: { job: JobInfo }) {
  const update = useUpdateJob(job.job_id);
  const [draft, setDraft] = useState<string | null>(null);
  const cancel = () => {
    setDraft(null);
    update.reset();
  };

  if (draft !== null) {
    return (
      <form
        className="rename"
        onSubmit={(e) => {
          e.preventDefault();
          const label = draft.trim();
          update.mutate(
            {
              params: { path: { id: job.job_id } },
              body: { label: label === "" ? null : label },
            },
            { onSuccess: () => setDraft(null) },
          );
        }}
      >
        <input
          aria-label="Job name"
          placeholder="Unnamed Job"
          autoFocus
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && cancel()}
        />
        <button
          type="submit"
          className="icon-btn"
          title="Save name"
          aria-label="Save name"
          disabled={update.isPending}
        >
          <Check size={20} aria-hidden="true" />
        </button>
        <button
          type="button"
          className="icon-btn"
          title="Cancel"
          aria-label="Cancel renaming"
          onClick={cancel}
        >
          <X size={20} aria-hidden="true" />
        </button>
        <RequestError
          error={update.error}
          messages={{ 403: "You are not allowed to rename this job." }}
        />
      </form>
    );
  }

  return (
    <>
      <h1>{job.label ?? <em className="muted">Unnamed Job</em>}</h1>
      {job.permissions.includes("manage") && (
        <button
          type="button"
          className="icon-btn"
          title="Rename job"
          aria-label="Rename job"
          onClick={() => setDraft(job.label ?? "")}
        >
          <Pencil size={18} aria-hidden="true" />
        </button>
      )}
    </>
  );
}

/** Who owns the job, where it runs, and the last thing that happened to it. */
function JobContext({
  job,
  hostName,
}: {
  job: JobInfo;
  hostName: string | undefined;
}) {
  // A group owner has no profile to fetch, and falls back to its short ID.
  const owner = $api.useQuery(
    "get",
    "/users/{id}",
    { params: { path: { id: job.owner_id ?? "" } } },
    { enabled: job.owner_id != null },
  );

  return (
    <p className="page-context">
      <span>
        Owner:{" "}
        {job.owner_id == null ? (
          <span className="muted">none</span>
        ) : (
          <EntityLink
            kind="user"
            id={job.owner_id}
            label={owner.data?.name}
            icon={User}
          />
        )}
      </span>
      <span>
        Host:{" "}
        {job.dispatched_on_host_id == null ? (
          <span className="muted">not assigned yet</span>
        ) : (
          <EntityLink
            kind="host"
            id={job.dispatched_on_host_id}
            label={hostName}
            icon={Server}
          />
        )}
      </span>
      <span>
        {job.terminated_at != null ? (
          <>
            ended <RelTime iso={job.terminated_at} />
          </>
        ) : job.started_at != null ? (
          <>
            started <RelTime iso={job.started_at} />
          </>
        ) : (
          <>
            queued <RelTime iso={job.queued_at} />
          </>
        )}
      </span>
    </p>
  );
}
