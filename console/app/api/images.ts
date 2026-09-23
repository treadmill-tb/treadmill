// The console calls an image set an "image", its generations "versions" and
// their members "variants".
import { client } from "./client";
import { ApiError } from "./errors";
import type { components } from "./schema";
import { EVERYONE_SUBJECT, SYSTEM_SUBJECT } from "./subjects";

type ImageInfo = components["schemas"]["ImageInfo"];
type ImageSetInfo = components["schemas"]["ImageSetInfo"];
type ImageSourceInfo = components["schemas"]["ImageSourceInfo"];

export type Variant = {
  manifest_digest: string;
  platform_profile: string;
  predicate?: string | null;
};

export function isStandard(set: Pick<ImageSetInfo, "owner_id">): boolean {
  return set.owner_id === SYSTEM_SUBJECT;
}

export function shortDigest(digest: string): string {
  return digest.startsWith("sha256:")
    ? `sha256:${digest.slice(7, 19)}…`
    : `${digest.slice(0, 19)}…`;
}

export function buildName(digest: string, image: ImageInfo | undefined) {
  if (image?.title != null && image.title !== "") return image.title;
  const source = image?.sources[0];
  if (source !== undefined) return `${source.registry}/${source.repository}`;
  return shortDigest(digest);
}

/** Parses `registry/repository[:tag]@sha256:…`. Tags can't be resolved
 * without the registry, so the digest is required. */
export function parseReference(
  reference: string,
):
  { registry: string; repository: string; digest: string } | { error: string } {
  const trimmed = reference.trim();
  const at = trimmed.lastIndexOf("@");
  if (at < 0) return { error: "Missing digest (…@sha256:…)." };
  const name = trimmed.slice(0, at);
  const digest = trimmed.slice(at + 1);
  if (!/^sha256:[0-9a-f]{64}$/.test(digest)) {
    return { error: "Invalid digest." };
  }
  const slash = name.indexOf("/");
  if (slash <= 0 || slash === name.length - 1) {
    return { error: "Missing registry or repository." };
  }
  const repository = name.slice(slash + 1).replace(/:[^/]*$/, "");
  return { registry: name.slice(0, slash), repository, digest };
}

function variantKey(v: Variant): string {
  return `${v.platform_profile}\u0000${v.predicate ?? ""}`;
}

/** "added X; updated Y" between two versions; `prev` undefined for the
 * first. */
export function describeChanges(
  prev: Variant[] | undefined,
  next: Variant[],
): string {
  if (prev === undefined) {
    return next.length === 1 ? "1 variant" : `${next.length} variants`;
  }
  const before = new Map(prev.map((v) => [variantKey(v), v]));
  const after = new Map(next.map((v) => [variantKey(v), v]));
  const added = new Set<string>();
  const updated = new Set<string>();
  const removed = new Set<string>();
  for (const [key, v] of after) {
    const old = before.get(key);
    if (old === undefined) added.add(v.platform_profile);
    else if (old.manifest_digest !== v.manifest_digest)
      updated.add(v.platform_profile);
  }
  for (const [key, v] of before) {
    if (!after.has(key)) removed.add(v.platform_profile);
  }
  const parts = [
    added.size > 0 && `added ${[...added].join(", ")}`,
    updated.size > 0 && `updated ${[...updated].join(", ")}`,
    removed.size > 0 && `removed ${[...removed].join(", ")}`,
  ].filter((p): p is string => typeof p === "string");
  if (parts.length > 0) return parts.join("; ");
  const reordered = prev.some(
    (v, i) => variantKey(v) !== variantKey(next[i] ?? v),
  );
  return reordered ? "reordered" : "unchanged";
}

function check<T>(result: {
  data?: T;
  error?: unknown;
  response: Response;
}): T {
  if (!result.response.ok || result.data === undefined) {
    throw new ApiError(result.response.status, result.error);
  }
  return result.data;
}

const STATUS_ORDER = ["system", "canonical", "external"];

function managedSources(image: ImageInfo): ImageSourceInfo[] {
  return image.sources
    .filter((s) => s.permissions.includes("manage"))
    .sort(
      (a, b) =>
        ((STATUS_ORDER.indexOf(a.status) + 4) % 4) -
        ((STATUS_ORDER.indexOf(b.status) + 4) % 4),
    );
}

export type AccessPlan = {
  grants: { digest: string; sourceId: string; subject: string }[];
  /** No source the viewer manages; their owners may have shared them. */
  blocked: Variant[];
};

/** The source grants that let every subject pull every variant's build. */
export async function planAccess(
  variants: Variant[],
  subjects: string[],
): Promise<AccessPlan> {
  const plan: AccessPlan = { grants: [], blocked: [] };
  if (subjects.length === 0) return plan;
  const seen = new Set<string>();
  for (const variant of variants) {
    const digest = variant.manifest_digest;
    if (seen.has(digest)) continue;
    seen.add(digest);

    const image = await client.GET("/images/{digest}", {
      params: { path: { digest } },
    });
    if (image.response.status === 404) {
      plan.blocked.push(variant);
      continue;
    }
    const sources = managedSources(check(image));
    const target = sources[0];
    if (target === undefined) {
      plan.blocked.push(variant);
      continue;
    }

    const holders = new Set<string>();
    for (const source of sources) {
      const grants = check(
        await client.GET("/images/{digest}/sources/{source_id}/grants", {
          params: { path: { digest, source_id: source.id } },
        }),
      );
      for (const g of grants) {
        if (g.permission === "use") holders.add(g.subject_id);
      }
    }
    if (holders.has(EVERYONE_SUBJECT)) continue;
    for (const subject of subjects) {
      if (!holders.has(subject)) {
        plan.grants.push({ digest, sourceId: target.id, subject });
      }
    }
  }
  return plan;
}

/** Idempotent, so a partly failed run can be repeated. */
export async function applyAccess(plan: AccessPlan): Promise<void> {
  for (const { digest, sourceId, subject } of plan.grants) {
    const result = await client.POST(
      "/images/{digest}/sources/{source_id}/grants",
      {
        params: { path: { digest, source_id: sourceId } },
        body: { subject_id: subject, permission: "use" },
      },
    );
    if (!result.response.ok) {
      throw new ApiError(result.response.status, result.error);
    }
  }
}

/** Registers the build from this source unless the viewer already sees it
 * there. */
export async function ensureImage(
  registry: string,
  repository: string,
  digest: string,
): Promise<ImageInfo> {
  const existing = await client.GET("/images/{digest}", {
    params: { path: { digest } },
  });
  if (existing.response.ok && existing.data !== undefined) {
    const has = existing.data.sources.some(
      (s) => s.registry === registry && s.repository === repository,
    );
    if (has) return existing.data;
  } else if (existing.response.status !== 404) {
    throw new ApiError(existing.response.status, existing.error);
  }
  return check(
    await client.POST("/images/{digest}/sources", {
      params: { path: { digest } },
      body: { registry, repository },
    }),
  );
}

/** Everyone holding `use` on a set, `everyone` included. */
export async function setUsers(setId: string): Promise<string[]> {
  const grants = check(
    await client.GET("/image-sets/{id}/grants", {
      params: { path: { id: setId } },
    }),
  );
  return grants.filter((g) => g.permission === "use").map((g) => g.subject_id);
}

/** Shares the builds with the set's users first; unless `force`, publishes
 * nothing if some can't be shared. */
export async function publishVersion(
  setId: string,
  variants: Variant[],
  force: boolean,
): Promise<{ blocked: Variant[] } | { generation: number }> {
  const plan = await planAccess(variants, await setUsers(setId));
  if (plan.blocked.length > 0 && !force) return { blocked: plan.blocked };
  await applyAccess(plan);
  const created = check(
    await client.POST("/image-sets/{id}/generations", {
      params: { path: { id: setId } },
      body: {
        members: variants.map((v) => ({
          manifest_digest: v.manifest_digest,
          platform_profile: v.platform_profile,
          predicate: v.predicate ?? null,
        })),
      },
    }),
  );
  return { generation: created.generation };
}

/** Returns the variants it couldn't fix. */
export async function fixAccess(
  setId: string,
  variants: Variant[],
): Promise<Variant[]> {
  const plan = await planAccess(variants, await setUsers(setId));
  await applyAccess(plan);
  return plan.blocked;
}
