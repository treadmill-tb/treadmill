import { Link } from "react-router";

import { $api } from "../api/client";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { SubjectName } from "../components/subject";
import { VariantList } from "../components/variant-list";
import { RestoreButton } from "./image";
import type { Route } from "./+types/image-version";

export default function ImageVersion({ params }: Route.ComponentProps) {
  const n = Number(params.n);
  const valid = Number.isInteger(n) && n >= 1;
  const set = $api.useQuery("get", "/image-sets/{id}", {
    params: { path: { id: params.id } },
  });
  const version = $api.useQuery(
    "get",
    "/image-sets/{id}/generations/{n}",
    { params: { path: { id: params.id, n } } },
    { enabled: valid },
  );
  const grants = $api.useQuery(
    "get",
    "/image-sets/{id}/grants",
    { params: { path: { id: params.id } } },
    { retry: false },
  );

  if (!valid) return <p className="error">There is no such version.</p>;
  const latest = set.data?.latest_generation;

  return (
    <>
      <header className="page-head">
        <div className="page-head-title">
          <h1>
            <Link to={`/images/${params.id}`}>
              {set.data?.display_name ?? "Image"}
            </Link>{" "}
            v{n}
          </h1>
          {latest === n && <span className="badge ok">current</span>}
        </div>
        <div className="page-head-actions">
          {version.data !== undefined && latest !== n && grants.isSuccess && (
            <RestoreButton setId={params.id} version={version.data} />
          )}
        </div>
      </header>
      <RequestError
        error={set.error ?? version.error}
        messages={{
          404: "No such version, or it isn't shared with you.",
        }}
      />
      {version.data !== undefined && (
        <>
          <p className="page-context">
            <span>
              <RelTime iso={version.data.created_at} />
            </span>
            {version.data.created_by != null && (
              <span>
                <SubjectName id={version.data.created_by} />
              </span>
            )}
            {latest != null && latest !== n && (
              <span>
                Current: <Link to={`/images/${params.id}`}>v{latest}</Link>
              </span>
            )}
          </p>
          <section>
            <h2>Variants</h2>
            <VariantList
              variants={version.data.members}
              showAccess={grants.isSuccess}
            />
          </section>
        </>
      )}
    </>
  );
}
