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
import { JobServices } from "../components/job-services";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";
import { Tags } from "../components/tags";
import { useResourceWatch } from "../hooks/use-resource-watch";
import type { Route } from "./+types/job-detail";

const LEASE_PROMPT =
  'New lease: "2h" to set it, "+30m" / "-10m" to extend or shorten, ' +
  "or an ISO timestamp to end it at a fixed instant.";

function formatSeconds(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return [h && `${h}h`, m && `${m}m`, (s || !(h || m)) && `${s}s`]
    .filter(Boolean)
    .join(" ");
}

export default function JobDetail({ params }: Route.ComponentProps) {
  const queryClient = useQueryClient();
  useResourceWatch(`/jobs/${params.id}/watch`, ["get", "/jobs/{id}"]);
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
  const update = $api.useMutation("patch", "/jobs/{id}", {
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
          <MutationError error={update.error} />

          <dl className="props">
            <dt>Label</dt>
            <dd>
              {job.data.label ?? <span className="muted">—</span>}{" "}
              {job.data.permissions.includes("manage") && (
                <button
                  disabled={update.isPending}
                  onClick={() => {
                    const label = window.prompt(
                      "Job label (empty clears it):",
                      job.data.label ?? "",
                    );
                    if (label !== null) {
                      update.mutate({
                        params: { path: { id: params.id } },
                        body: { label: label === "" ? null : label },
                      });
                    }
                  }}
                >
                  Edit
                </button>
              )}
            </dd>
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
            <dt>Address</dt>
            <dd>
              {job.data.job_ip_address === null ||
              job.data.job_ip_address === undefined ? (
                <span className="muted">—</span>
              ) : (
                <span className="mono">{job.data.job_ip_address}</span>
              )}
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
            <dt>Lease</dt>
            <dd>
              {formatSeconds(job.data.lease_duration_secs)}
              {job.data.lease_expires_at != null && (
                <>
                  {" · expires "}
                  <RelTime iso={job.data.lease_expires_at} />
                </>
              )}{" "}
              {job.data.permissions.includes("manage") && (
                <button
                  disabled={update.isPending}
                  onClick={() => {
                    const lease = window.prompt(LEASE_PROMPT, "+30m");
                    if (lease !== null && lease !== "") {
                      update.mutate({
                        params: { path: { id: params.id } },
                        body: { lease },
                      });
                    }
                  }}
                >
                  Change
                </button>
              )}
            </dd>
            <dt>At lease expiry</dt>
            <dd>
              {job.data.lease_expiry_action === "preempt"
                ? "keep running; reclaim when a host is needed"
                : "terminate"}{" "}
              {job.data.permissions.includes("manage") && (
                <button
                  disabled={update.isPending}
                  onClick={() =>
                    update.mutate({
                      params: { path: { id: params.id } },
                      body: {
                        lease_expiry_action:
                          job.data.lease_expiry_action === "preempt"
                            ? "terminate"
                            : "preempt",
                      },
                    })
                  }
                >
                  {job.data.lease_expiry_action === "preempt"
                    ? "Terminate instead"
                    : "Allow reclaim instead"}
                </button>
              )}
            </dd>
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

          <JobServices
            jobId={params.id}
            services={job.data.services}
            canOpen={
              job.data.permissions.includes("manage") &&
              job.data.state !== "finalized"
            }
          />

          <JobLog
            key={`${params.id} ${replayBytes}`}
            jobId={params.id}
            dispatched={job.data.dispatched_on_host_id != null}
            replayBytes={replayBytes}
            canSendInput={
              job.data.permissions.includes("manage") &&
              job.data.state !== "finalized"
            }
          />

          <AuditLog entity="jobs" id={params.id} />
        </>
      )}
    </>
  );
}
