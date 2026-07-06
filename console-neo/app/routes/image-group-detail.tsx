import { Link } from "react-router";

import { $api } from "../api/client";
import { AuditLog } from "../components/audit-log";
import { Digest } from "../components/digest";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";
import { Tags } from "../components/tags";
import type { Route } from "./+types/image-group-detail";

export default function ImageGroupDetail({ params }: Route.ComponentProps) {
  const group = $api.useQuery("get", "/image-groups/{id}", {
    params: { path: { id: params.id } },
  });
  const grants = $api.useQuery("get", "/image-groups/{id}/grants", {
    params: { path: { id: params.id } },
  });

  const latest = group.data?.latest_generation;
  const generation = $api.useQuery(
    "get",
    "/image-groups/{id}/generations/{n}",
    { params: { path: { id: params.id, n: latest ?? 0 } } },
    { enabled: latest != null },
  );

  return (
    <>
      {group.isPending && <p className="muted">Loading…</p>}
      {group.isError && <p className="error">Failed to load the group.</p>}
      {group.data && (
        <>
          <div className="toolbar">
            <h1>Group {group.data.name}</h1>
            <span className="spacer" />
            {/* TODO(console-neo): wire PUT /image-groups/{id}/public and
                POST /image-groups/{id}/generations */}
            <button disabled title="Not implemented yet">
              {group.data.public ? "Make private" : "Make public"}
            </button>
            <button disabled title="Not implemented yet">
              New generation
            </button>
          </div>

          <dl className="props">
            <dt>Id</dt>
            <dd className="mono">{group.data.id}</dd>
            <dt>Label</dt>
            <dd>{group.data.label ?? <span className="muted">—</span>}</dd>
            <dt>Visibility</dt>
            <dd>
              <span className={`badge ${group.data.public ? "warn" : ""}`}>
                {group.data.public ? "public" : "private"}
              </span>
            </dd>
            <dt>Owner</dt>
            <dd>
              <EntityLink kind="user" id={group.data.owner_id} />
            </dd>
            <dt>Created</dt>
            <dd>
              <RelTime iso={group.data.created_at} />
            </dd>
          </dl>

          <section>
            <h2>
              Latest generation
              {latest != null && (
                <>
                  {" "}
                  <Link
                    to={`/image-groups/${params.id}/generations/${latest}`}
                    className="mono"
                  >
                    #{latest}
                  </Link>
                </>
              )}
            </h2>
            {latest == null && <p className="muted">No generations yet.</p>}
            {latest != null && generation.isPending && (
              <p className="muted">Loading…</p>
            )}
            {generation.isError && (
              <p className="error">Failed to load the generation.</p>
            )}
            {generation.data && (
              <GenerationMembers members={generation.data.members} />
            )}
          </section>

          <section>
            <div className="toolbar">
              <h2>Grants</h2>
              <span className="spacer" />
              {/* TODO(console-neo): wire POST /image-groups/{id}/grants and
                  DELETE /image-groups/{id}/grants/{subject_id}/{permission} */}
              <button disabled title="Not implemented yet">
                Grant
              </button>
            </div>
            {grants.isPending && <p className="muted">Loading…</p>}
            {grants.isError && (
              <p className="error">Failed to load the grants.</p>
            )}
            {grants.data &&
              (grants.data.length === 0 ? (
                <p className="muted">No explicit grants.</p>
              ) : (
                <table>
                  <thead>
                    <tr>
                      <th>Subject</th>
                      <th>Permission</th>
                    </tr>
                  </thead>
                  <tbody>
                    {grants.data.map((grant) => (
                      <tr key={`${grant.subject_id}/${grant.permission}`}>
                        <td>
                          <EntityLink kind="user" id={grant.subject_id} />
                        </td>
                        <td>
                          <span className="badge">{grant.permission}</span>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              ))}
          </section>

          <AuditLog entity="image-groups" id={params.id} />
        </>
      )}
    </>
  );
}

export function GenerationMembers({
  members,
}: {
  members: {
    index: number;
    image_id: string;
    manifest_digest: string;
    required_host_tags: string[];
  }[];
}) {
  if (members.length === 0) {
    return <p className="muted">No members.</p>;
  }
  return (
    <table>
      <thead>
        <tr>
          <th>#</th>
          <th>Image</th>
          <th>Digest</th>
          <th>Required host tags</th>
        </tr>
      </thead>
      <tbody>
        {members.map((m) => (
          <tr key={m.index}>
            <td>{m.index}</td>
            <td>
              <Link to={`/images/${m.manifest_digest}`} className="mono">
                {m.image_id.slice(0, 8)}
              </Link>
            </td>
            <td>
              <Digest digest={m.manifest_digest} />
            </td>
            <td>
              <Tags tags={m.required_host_tags} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
