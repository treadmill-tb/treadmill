import { Link } from "react-router";

import { $api } from "../api/client";
import { shortDigest } from "../api/images";
import type { components } from "../api/schema";
import { EntityLink, shortId } from "./entity-link";

type JobImage = components["schemas"]["JobImage"];
type JobPredecessor = components["schemas"]["JobPredecessor"];

function ImageVersionRef({
  setId,
  generation,
}: {
  setId: string;
  generation: number;
}) {
  const set = $api.useQuery("get", "/image-sets/{id}", {
    params: { path: { id: setId } },
  });
  return (
    <Link to={`/images/${setId}/versions/${generation}`} title={setId}>
      {set.data?.display_name ?? shortId(setId)} v{generation}
    </Link>
  );
}

export function ImageRef({
  image,
  predecessor,
}: {
  image: JobImage;
  predecessor?: JobPredecessor | null;
}) {
  const ref = image.reference;
  return (
    <span>
      {ref.type === "image" ? (
        <Link
          to={`/images/build/${ref.manifest_digest}`}
          className="mono"
          title={ref.manifest_digest}
        >
          {shortDigest(ref.manifest_digest)}
        </Link>
      ) : (
        <ImageVersionRef setId={ref.set_id} generation={ref.generation} />
      )}
      {predecessor != null && (
        <span className="muted">
          {" "}
          ({predecessor.type} <EntityLink kind="job" id={predecessor.job_id} />)
        </span>
      )}
    </span>
  );
}
