import { Link } from "react-router";

import { $api } from "../api/client";
import { LiveBadge } from "../components/badges";
import { AuditLog } from "../components/audit-log";
import { HostSpecView } from "../components/host-spec";
import { RelTime } from "../components/rel-time";
import { useResourceWatch } from "../hooks/use-resource-watch";
import type { Route } from "./+types/host-detail";

export default function HostDetail({ params }: Route.ComponentProps) {
  const host = $api.useQuery("get", "/hosts/{id}", {
    params: { path: { id: params.id } },
  });
  useResourceWatch(`/hosts/${params.id}/watch`, [
    "get",
    "/hosts/{id}",
    { params: { path: { id: params.id } } },
  ]);

  return (
    <>
      {host.isPending && <p className="muted">Loading…</p>}
      {host.isError && (
        <p className="error">No such host, or you cannot read it.</p>
      )}
      {host.data && (
        <>
          <h1>
            Host {host.data.name} <LiveBadge live={host.data.live} />
            {host.data.maintenance && (
              <span className="badge warn">maintenance</span>
            )}
          </h1>
          <dl className="props">
            <dt>Id</dt>
            <dd className="mono">{host.data.host_id}</dd>
            <dt>Last seen</dt>
            <dd>
              <RelTime iso={host.data.last_seen_at} />
            </dd>
            <dt>Spec revision</dt>
            <dd>
              {host.data.spec_revision ?? <span className="muted">—</span>}
            </dd>
          </dl>

          <section>
            <h2>Spec</h2>
            {host.data.permissions.includes("manage") && (
              <div className="toolbar">
                <Link className="btn" to={`/hosts/${params.id}/spec`}>
                  {host.data.spec == null ? "Write a spec" : "Edit spec"}
                </Link>
              </div>
            )}
            {host.data.spec == null ? (
              <p className="muted">
                This host has no spec. Nothing can be scheduled onto it: there
                is no description to evaluate a job&rsquo;s predicate against,
                and no platform profile for an image set to match.
              </p>
            ) : (
              <HostSpecView spec={host.data.spec} />
            )}
          </section>

          <AuditLog entity="hosts" id={params.id} />
        </>
      )}
    </>
  );
}
