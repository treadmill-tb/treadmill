import type { Tone } from "../components/badges";
import type { components } from "./schema";

type HostListEntry = components["schemas"]["HostListEntry"];
type JobInfo = components["schemas"]["JobInfo"];
type JobInitSpec = components["schemas"]["JobInitSpec"];

export type HostStatus = "free" | "busy" | "maintenance" | "offline";

export function hostStatus(host: HostListEntry | undefined): HostStatus {
  if (host === undefined || !host.live) return "offline";
  if (host.maintenance) return "maintenance";
  return host.busy ? "busy" : "free";
}

export const STATUS_TONE: Record<HostStatus, Tone> = {
  free: "ok",
  busy: "warn",
  maintenance: "",
  offline: "danger",
};

type SpecSchema = {
  properties?: { spec_version?: { $ref?: string } };
  $defs?: Record<string, { enum?: unknown[] }>;
};

export function specVersion(schema: unknown): string | undefined {
  const { properties, $defs } = (schema ?? {}) as SpecSchema;
  const name = properties?.spec_version?.$ref?.replace("#/$defs/", "");
  const version = name === undefined ? undefined : $defs?.[name]?.enum?.[0];
  return typeof version === "string" ? version : undefined;
}

export function singleHostPredicate(hostId: string): string {
  return `host.id == "${hostId}"`;
}

export function parseSingleHostPredicate(predicate: string): string | null {
  return /^host\.id == "([0-9a-f-]{36})"$/.exec(predicate.trim())?.[1] ?? null;
}

export function jobImageSpec(job: JobInfo): JobInitSpec {
  const reference = job.image.reference;
  return reference.type === "image_set"
    ? {
        type: "image_set",
        set_id: reference.set_id,
        generation: reference.generation,
      }
    : { type: "image", manifest_digest: reference.manifest_digest };
}
