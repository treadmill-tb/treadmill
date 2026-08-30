import { $api } from "../api/client";
import { LiveBadge } from "../components/badges";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";

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
                <th>Site</th>
                <th>Platform profiles</th>
                <th>DUTs</th>
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
                    {host.maintenance && (
                      <span className="badge warn">maintenance</span>
                    )}
                  </td>
                  {/* An undescribed host has nothing to show and cannot be
                      scheduled onto; say so rather than showing blanks. */}
                  {host.spec == null ? (
                    <td colSpan={3} className="muted">
                      no spec
                    </td>
                  ) : (
                    <>
                      <td>{host.spec.site}</td>
                      <td>
                        {host.spec.platform.profiles.map((p) => (
                          <span key={p} className="chip mono">
                            {p}
                          </span>
                        ))}
                      </td>
                      <td>{host.spec.duts.length}</td>
                    </>
                  )}
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
