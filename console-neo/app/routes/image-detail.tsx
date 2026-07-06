import { $api } from "../api/client";
import { Digest } from "../components/digest";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";
import type { Route } from "./+types/image-detail";

export default function ImageDetail({ params }: Route.ComponentProps) {
  const image = $api.useQuery("get", "/images/{digest}", {
    params: { path: { digest: params.digest } },
  });

  return (
    <>
      <h1>Image</h1>
      {image.isPending && <p className="muted">Loading…</p>}
      {image.isError && <p className="error">Failed to load the image.</p>}
      {image.data && (
        <>
          <dl className="props">
            <dt>Label</dt>
            <dd>{image.data.label ?? <span className="muted">—</span>}</dd>
            <dt>Id</dt>
            <dd className="mono">{image.data.id}</dd>
            <dt>Digest</dt>
            <dd>
              <Digest digest={image.data.manifest_digest} />
            </dd>
            <dt>Artifact type</dt>
            <dd className="mono">{image.data.artifact_type}</dd>
            <dt>Owner</dt>
            <dd>
              <EntityLink kind="user" id={image.data.owner_id} />
            </dd>
            <dt>Registered</dt>
            <dd>
              <RelTime iso={image.data.created_at} />
            </dd>
          </dl>

          <section>
            <h2>Locations</h2>
            <table>
              <thead>
                <tr>
                  <th>Registry</th>
                  <th>Repository</th>
                  <th>Status</th>
                </tr>
              </thead>
              <tbody>
                {image.data.locations.map((loc) => (
                  <tr key={`${loc.registry}/${loc.repository}`}>
                    <td className="mono">{loc.registry}</td>
                    <td className="mono">{loc.repository}</td>
                    <td>
                      <span className="badge">{loc.status}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
        </>
      )}
    </>
  );
}
