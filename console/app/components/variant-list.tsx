import type { ReactNode } from "react";

import { $api } from "../api/client";
import { buildName, type Variant } from "../api/images";
import type { components } from "../api/schema";
import { Digest } from "./digest";

type GenerationMemberInfo = components["schemas"]["GenerationMemberInfo"];

export function BuildName({ digest }: { digest: string }) {
  const image = $api.useQuery(
    "get",
    "/images/{digest}",
    { params: { path: { digest } } },
    { retry: false },
  );
  return <span title={digest}>{buildName(digest, image.data)}</span>;
}

function BuildDetails({ digest }: { digest: string }) {
  const image = $api.useQuery(
    "get",
    "/images/{digest}",
    { params: { path: { digest } } },
    { retry: false },
  );
  return (
    <small className="muted variant-details">
      {image.data?.sources.map((s) => (
        <span key={s.id} className="mono">
          {s.registry}/{s.repository}
        </span>
      ))}
      <Digest digest={digest} />
    </small>
  );
}

/** Variants in selection order. Predicates are shown verbatim. */
export function VariantList({
  variants,
  showAccess = false,
  actions,
  marks,
}: {
  variants: (Variant | GenerationMemberInfo)[];
  /** Flag builds not everyone the image is shared with can pull. */
  showAccess?: boolean;
  actions?: (index: number) => ReactNode;
  marks?: (index: number) => string | null;
}) {
  if (variants.length === 0) return <p className="muted">None</p>;
  return (
    <ol className="variant-list">
      {variants.map((v, i) => {
        const mark = marks?.(i) ?? null;
        const access = "usable" in v ? v : null;
        return (
          <li key={i}>
            <span className="variant-index">{i + 1}</span>
            <div className="variant-body">
              <div className="variant-head">
                <span className="chip mono">{v.platform_profile}</span>
                <BuildName digest={v.manifest_digest} />
                {mark !== null && <span className="badge active">{mark}</span>}
                {access !== null && !access.usable && (
                  <span
                    className="badge danger"
                    title="You can't pull this build."
                  >
                    unavailable to you
                  </span>
                )}
                {showAccess &&
                  access !== null &&
                  access.usable &&
                  !access.usable_by_grantees && (
                    <span
                      className="badge warn"
                      title="Someone this image is shared with can't pull this build."
                    >
                      not shared with everyone
                    </span>
                  )}
              </div>
              {v.predicate != null && v.predicate !== "" && (
                <small className="muted">
                  only where <code>{v.predicate}</code>
                </small>
              )}
              <BuildDetails digest={v.manifest_digest} />
            </div>
            {actions !== undefined && (
              <span className="variant-actions">{actions(i)}</span>
            )}
          </li>
        );
      })}
    </ol>
  );
}
