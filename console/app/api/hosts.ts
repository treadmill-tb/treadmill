import type { Tone } from "../components/badges";
import type { components } from "./schema";

type HostListEntry = components["schemas"]["HostListEntry"];

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

export function singleHostPredicate(hostId: string): string {
  return `host.id == "${hostId}"`;
}

export function parseSingleHostPredicate(predicate: string): string | null {
  return /^host\.id == "([0-9a-f-]{36})"$/.exec(predicate.trim())?.[1] ?? null;
}
