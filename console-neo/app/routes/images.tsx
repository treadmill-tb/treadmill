import { Link } from "react-router";

import { $api } from "../api/client";
import { Digest } from "../components/digest";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";

export default function Images() {
  const images = $api.useQuery("get", "/images");

  return (
    <>
      <div className="toolbar">
        <h1>Images</h1>
        <span className="spacer" />
        {/* TODO(console-neo): wire POST /images */}
        <button disabled title="Not implemented yet">
          Register image
        </button>
      </div>
      {images.isPending && <p className="muted">Loading…</p>}
      {images.isError && <p className="error">Failed to load images.</p>}
      {images.data &&
        (images.data.length === 0 ? (
          <p className="muted">No images visible to this account.</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Label</th>
                <th>Digest</th>
                <th>Artifact type</th>
                <th>Owner</th>
                <th>Locations</th>
                <th>Registered</th>
              </tr>
            </thead>
            <tbody>
              {images.data.map((img) => (
                <tr key={img.id}>
                  <td>
                    <Link to={`/images/${img.manifest_digest}`}>
                      {img.label ?? <span className="mono">{img.id}</span>}
                    </Link>
                  </td>
                  <td>
                    <Digest digest={img.manifest_digest} />
                  </td>
                  <td className="mono">{img.artifact_type}</td>
                  <td>
                    <EntityLink kind="user" id={img.owner_id} />
                  </td>
                  <td>{img.locations.length}</td>
                  <td>
                    <RelTime iso={img.created_at} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ))}
    </>
  );
}
