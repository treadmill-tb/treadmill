import { useQueryClient } from "@tanstack/react-query";
import { useSearchParams } from "react-router";

import { $api } from "../api/client";
import {
  JobStateBadge,
  TaskExitBadge,
  TerminationBadge,
} from "../components/badges";
import { AuditLog } from "../components/audit-log";
import { Digest } from "../components/digest";
import { EntityLink } from "../components/entity-link";
import { ImageRef } from "../components/image-ref";
import { JobLog, parseReplayBytes } from "../components/job-log";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";
import { Tags } from "../components/tags";
import type { Route } from "./+types/job-detail";

export default function JobDetail({ params }: Route.ComponentProps) {
  const queryClient = useQueryClient();
  // Per-page-load override for how much log history to replay (a user
  // settings page may subsume this later).
  const [searchParams] = useSearchParams();
  const replayBytes = parseReplayBytes(searchParams.get("replay"));
  const job = $api.useQuery("get", "/jobs/{id}", {
    params: { path: { id: params.id } },
  });
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

  return (
    <>
      <h1>
        Job <span className="mono">{params.id}</span>
      </h1>
      {job.isPending && <p className="muted">Loading…</p>}
      {job.isError && <p className="error">Failed to load the job.</p>}
      {job.data && (
        <>
          <div className="toolbar">
            <JobStateBadge
              state={job.data.state}
              stage={job.data.initializing_stage}
            />
            <button
              className="danger"
              disabled={job.data.state === "finalized" || terminate.isPending}
              onClick={() => {
                if (window.confirm(`Terminate job ${params.id}?`)) {
                  terminate.mutate({ params: { path: { id: params.id } } });
                }
              }}
            >
              {terminate.isPending ? "Terminating…" : "Terminate"}
            </button>
          </div>
          <MutationError error={terminate.error} />

          <dl className="props">
            <dt>Image</dt>
            <dd>
              <ImageRef image={job.data.image} />
            </dd>
            <dt>Resolved digest</dt>
            <dd>
              <Digest digest={job.data.resolved_image_digest} />
            </dd>
            <dt>Owner</dt>
            <dd>
              <EntityLink kind="user" id={job.data.owner_id} />
            </dd>
            <dt>Host</dt>
            <dd>
              <EntityLink kind="host" id={job.data.dispatched_on_host_id} />
            </dd>
            <dt>Queued</dt>
            <dd>
              <RelTime iso={job.data.queued_at} />
            </dd>
            <dt>Started</dt>
            <dd>
              <RelTime iso={job.data.started_at} />
            </dd>
            <dt>Terminated</dt>
            <dd>
              <RelTime iso={job.data.terminated_at} />
            </dd>
            <dt>Timeout</dt>
            <dd>{job.data.timeout_secs}s</dd>
            <dt>Restarts left</dt>
            <dd>{job.data.restart_policy.remaining_restarts}</dd>
            <dt>Host tags required</dt>
            <dd>
              <Tags tags={job.data.host_tag_requirements} />
            </dd>
            <dt>Target requirements</dt>
            <dd>
              {job.data.target_requirements.length === 0 ? (
                <span className="muted">—</span>
              ) : (
                job.data.target_requirements.map((tags, i) => (
                  <div key={i}>
                    target {i}: <Tags tags={tags} />
                  </div>
                ))
              )}
            </dd>
            <dt>Outcome</dt>
            <dd>
              <TaskExitBadge status={job.data.task_exit_status} />{" "}
              <TerminationBadge reason={job.data.termination_reason} />
              {job.data.exit_message != null && (
                <div className="muted">{job.data.exit_message}</div>
              )}
            </dd>
            <dt>SSH</dt>
            <dd>
              {job.data.ssh_endpoints == null ||
              job.data.ssh_endpoints.length === 0 ? (
                <span className="muted">—</span>
              ) : (
                job.data.ssh_endpoints.map((ep) => (
                  <div key={`${ep.ssh_host}:${ep.ssh_port}`} className="mono">
                    {ep.ssh_host}:{ep.ssh_port}
                  </div>
                ))
              )}
            </dd>
            <dt>SSH keys</dt>
            <dd>{job.data.ssh_keys.length} deployed</dd>
          </dl>

          <section>
            <h2>Parameters</h2>
            {Object.keys(job.data.parameters).length === 0 ? (
              <p className="muted">No parameters.</p>
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Value</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(job.data.parameters).map(([name, p]) => (
                    <tr key={name}>
                      <td className="mono">{name}</td>
                      <td>
                        {p.secret ? (
                          <span className="badge warn" title="Value withheld">
                            secret
                          </span>
                        ) : (
                          <span className="mono">{p.value}</span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>

          <JobLog jobId={params.id} replayBytes={replayBytes} />

          <AuditLog entity="jobs" id={params.id} />
        </>
      )}
    </>
  );
}
