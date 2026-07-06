import { $api } from "../api/client";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";

export default function ImageGroups() {
  const groups = $api.useQuery("get", "/image-groups");

  return (
    <>
      <div className="toolbar">
        <h1>Image groups</h1>
        <span className="spacer" />
        {/* TODO(console-neo): wire POST /image-groups */}
        <button disabled title="Not implemented yet">
          Create group
        </button>
      </div>
      {groups.isPending && <p className="muted">Loading…</p>}
      {groups.isError && <p className="error">Failed to load image groups.</p>}
      {groups.data &&
        (groups.data.length === 0 ? (
          <p className="muted">No image groups visible to this account.</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Label</th>
                <th>Visibility</th>
                <th>Latest generation</th>
                <th>Owner</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {groups.data.map((g) => (
                <tr key={g.id}>
                  <td>
                    <EntityLink kind="imageGroup" id={g.id} label={g.name} />
                  </td>
                  <td>{g.label ?? <span className="muted">—</span>}</td>
                  <td>
                    <span className={`badge ${g.public ? "warn" : ""}`}>
                      {g.public ? "public" : "private"}
                    </span>
                  </td>
                  <td>
                    {g.latest_generation ?? <span className="muted">—</span>}
                  </td>
                  <td>
                    <EntityLink kind="user" id={g.owner_id} />
                  </td>
                  <td>
                    <RelTime iso={g.created_at} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ))}
    </>
  );
}
