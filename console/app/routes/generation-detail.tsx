import { $api } from "../api/client";
import { EntityLink } from "../components/entity-link";
import { RelTime } from "../components/rel-time";
import { GenerationMembers } from "./image-set-detail";
import type { Route } from "./+types/generation-detail";

export default function GenerationDetail({ params }: Route.ComponentProps) {
  const n = Number(params.n);
  const generation = $api.useQuery(
    "get",
    "/image-sets/{id}/generations/{n}",
    { params: { path: { id: params.id, n } } },
    { enabled: Number.isInteger(n) && n >= 0 },
  );

  return (
    <>
      <h1>
        Generation <EntityLink kind="imageSet" id={params.id} />
        <span className="mono">#{params.n}</span>
      </h1>
      {!Number.isInteger(n) || n < 0 ? (
        <p className="error">Invalid generation number.</p>
      ) : (
        <>
          {generation.isPending && <p className="muted">Loading…</p>}
          {generation.isError && (
            <p className="error">Failed to load the generation.</p>
          )}
          {generation.data && (
            <>
              <dl className="props">
                <dt>Created</dt>
                <dd>
                  <RelTime iso={generation.data.created_at} />
                </dd>
                <dt>Created by</dt>
                <dd>
                  <EntityLink kind="user" id={generation.data.created_by} />
                </dd>
              </dl>
              <section>
                <h2>Members</h2>
                <GenerationMembers members={generation.data.members} />
              </section>
            </>
          )}
        </>
      )}
    </>
  );
}
