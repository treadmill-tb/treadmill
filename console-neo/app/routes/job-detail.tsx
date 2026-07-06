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
import { RelTime } from "../components/rel-time";
import { Tags } from "../components/tags";
import type { Route } from "./+types/job-detail";

export default function JobDetail({ params }: Route.ComponentProps) {
  const job = $api.useQuery("get", "/jobs/{id}", {
    params: { path: { id: params.id } },
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
            {/* TODO(console-neo): wire DELETE /jobs/{id} */}
            <button className="danger" disabled title="Not implemented yet">
              Terminate
            </button>
          </div>

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

          <section>
            <h2>Console log</h2>
            {/* TODO(console-neo): nats.ws + xterm.js live log viewer, see
                doc/log-streaming-plan.md §4b */}
            <p className="muted">Log streaming lands in a follow-up.</p>
          </section>

          <AuditLog entity="jobs" id={params.id} />
        </>
      )}
    </>
  );
}
