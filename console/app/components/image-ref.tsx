import { Link } from "react-router";

import type { components } from "../api/schema";
import { EntityLink, shortId } from "./entity-link";

type JobImageRef = components["schemas"]["JobImageRef"];

export function ImageRef({ image }: { image: JobImageRef }) {
  switch (image.type) {
    case "image":
      // Catalog images are addressed by digest in the UI routes; the id alone
      // is not navigable, so render it as plain text.
      return (
        <span title={image.image_id}>
          image <span className="mono">{shortId(image.image_id)}</span>
        </span>
      );
    case "image_set":
      return (
        <span>
          set{" "}
          <Link
            to={`/image-sets/${image.set_id}/generations/${image.generation}`}
            className="mono"
            title={image.set_id}
          >
            {shortId(image.set_id)}#{image.generation}
          </Link>
        </span>
      );
    case "resume":
      return (
        <span>
          resume <EntityLink kind="job" id={image.job_id} />
        </span>
      );
    case "restart":
      return (
        <span>
          restart <EntityLink kind="job" id={image.job_id} />
        </span>
      );
  }
}
