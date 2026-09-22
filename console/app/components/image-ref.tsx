import { Link } from "react-router";

import type { components } from "../api/schema";
import { EntityLink, shortId } from "./entity-link";

type JobImage = components["schemas"]["JobImage"];
type JobPredecessor = components["schemas"]["JobPredecessor"];

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
        <>
          image{" "}
          <Link
            to={`/images/${ref.manifest_digest}`}
            className="mono"
            title={ref.manifest_digest}
          >
            {ref.manifest_digest.slice(0, 19)}…
          </Link>
        </>
      ) : (
        <>
          set{" "}
          <Link
            to={`/image-sets/${ref.set_id}/generations/${ref.generation}`}
            className="short-id"
            title={ref.set_id}
          >
            {shortId(ref.set_id)}#{ref.generation}
          </Link>
        </>
      )}
      {predecessor != null && (
        <>
          {" "}
          ({predecessor.type} <EntityLink kind="job" id={predecessor.job_id} />)
        </>
      )}
    </span>
  );
}
