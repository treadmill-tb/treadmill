import { $api } from "../api/client";
import { LiveBadge } from "../components/badges";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";
import { Tags } from "../components/tags";

export default function Hosts() {
  const hosts = $api.useQuery("get", "/hosts");

  return (
    <>
      <h1>Hosts</h1>
      {hosts.isPending && <p className="muted">Loading…</p>}
      {hosts.isError && <p className="error">Failed to load hosts.</p>}
      {hosts.data &&
        (hosts.data.length === 0 ? (
          <p className="muted">No hosts registered.</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Liveness</th>
                <th>Tags</th>
                <th>Targets</th>
                <th>Last seen</th>
              </tr>
            </thead>
            <tbody>
              {hosts.data.map((host) => (
                <tr key={host.host_id}>
                  <td>
                    <EntityLink
                      kind="host"
                      id={host.host_id}
                      label={host.name}
                    />
                  </td>
                  <td>
                    <LiveBadge live={host.live} />
                  </td>
                  <td>
                    <Tags tags={host.tags} />
                  </td>
                  <td>
                    {host.targets.length === 0 ? (
                      <span className="muted">—</span>
                    ) : (
                      host.targets.map((t) => (
                        <span key={t.name} className="tag">
                          {t.name}
                        </span>
                      ))
                    )}
                  </td>
                  <td>
                    <RelTime iso={host.last_seen_at} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ))}
    </>
  );
}
