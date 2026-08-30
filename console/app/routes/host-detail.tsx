import { $api } from "../api/client";
import { LiveBadge } from "../components/badges";
import { AuditLog } from "../components/audit-log";
import { RelTime } from "../components/rel-time";
import { useResourceWatch } from "../hooks/use-resource-watch";
import type { Route } from "./+types/host-detail";

export default function HostDetail({ params }: Route.ComponentProps) {
  // There is no GET /hosts/{id}; the detail view derives from the full list.
  const hosts = $api.useQuery("get", "/hosts");
  const host = hosts.data?.find((h) => h.host_id === params.id);
  useResourceWatch(`/hosts/${params.id}/watch`, ["get", "/hosts"]);

  return (
    <>
      {hosts.isPending && <p className="muted">Loading…</p>}
      {hosts.isError && <p className="error">Failed to load hosts.</p>}
      {hosts.data && host === undefined && (
        <p className="error">No such host.</p>
      )}
      {host && (
        <>
          <h1>
            Host {host.name} <LiveBadge live={host.live} />
          </h1>
          <dl className="props">
            <dt>Id</dt>
            <dd className="mono">{host.host_id}</dd>
            <dt>Last seen</dt>
            <dd>
              <RelTime iso={host.last_seen_at} />
            </dd>
          </dl>

          <AuditLog entity="hosts" id={params.id} />
        </>
      )}
    </>
  );
}
