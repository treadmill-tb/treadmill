/**
 * A job's log views, and the demux feeding them.
 *
 * The supervisor declares what views a job has and how to render each on a
 * reserved `meta` channel, as an append log of cumulative declarations (see
 * `LogViewManifest` in `treadmill-rs`). Nothing here is fatal: a declaration
 * this client cannot read is skipped, and a channel nobody declared still gets
 * a plain text tab, so output is never dropped for want of a manifest.
 */

/** How a view's bytes are rendered. */
export type LogRender = "terminal" | "text";

/** How a view's bytes are interpreted. */
export type LogFormat = "raw" | "jsonl";

/** One tab: what to call it, how to render it, and which channels feed it. */
export type LogView = {
  id: string;
  label: string;
  render: LogRender;
  format: LogFormat;
  channels: string[];
  order: number;
  default: boolean;
  input: boolean;
};

/** Manifest version this client reads; a line declaring another is skipped. */
const MANIFEST_VERSION = 1;

/** Lines a text view keeps, mirroring the terminal's scrollback. */
export const LINE_CAP = 10_000;

/**
 * Retained per channel, so a view mounting after its channel's first bytes —
 * or remounting because a declaration replaced its fallback tab — still shows
 * what already arrived.
 */
const RETAINED_BYTES_PER_CHANNEL = 1 << 20;

/** Parse one `meta` line into the views it declares, or `null` if it declares
 * nothing this client can use. */
export function parseManifestLine(line: string): LogView[] | null {
  let doc: unknown;
  try {
    doc = JSON.parse(line);
  } catch {
    return null;
  }
  if (typeof doc !== "object" || doc === null) return null;
  const { version, views } = doc as { version?: unknown; views?: unknown };
  if (version !== MANIFEST_VERSION || !Array.isArray(views)) return null;

  const parsed: LogView[] = [];
  for (const view of views) {
    if (typeof view !== "object" || view === null) continue;
    const normalized = normalizeView(view as Record<string, unknown>);
    if (normalized !== null) parsed.push(normalized);
  }
  return parsed;
}

/** A declared view, with anything unrecognized falling back to plain text. */
function normalizeView(view: Record<string, unknown>): LogView | null {
  const channels = Array.isArray(view.channels)
    ? view.channels.filter((c): c is string => typeof c === "string")
    : [];
  if (typeof view.id !== "string" || channels.length === 0) return null;
  return {
    id: view.id,
    label: typeof view.label === "string" ? view.label : view.id,
    render: view.render === "terminal" ? "terminal" : "text",
    format: view.format === "jsonl" ? "jsonl" : "raw",
    channels,
    order: typeof view.order === "number" ? view.order : 0,
    default: view.default === true,
    input: view.input === true,
  };
}

/** The tab a channel gets while nothing has declared one for it. Sorts last:
 * a declared view is a better guess at what the user wants to see first. */
export function fallbackView(channel: string): LogView {
  return {
    id: channel,
    label: channel,
    render: "text",
    format: "raw",
    channels: [channel],
    order: Number.MAX_SAFE_INTEGER,
    default: false,
    input: false,
  };
}

/**
 * The tabs to show: every declared view, plus a fallback for each channel that
 * has produced bytes without being declared. Each channel feeds exactly one
 * view — the first that claims it — so two declarations naming the same
 * channel cannot render it twice.
 */
export function resolveViews(
  declared: Iterable<LogView>,
  seen: Iterable<string>,
): LogView[] {
  const ordered = [...declared].sort(byOrder);
  const covered = new Set(ordered.flatMap((view) => view.channels));
  for (const channel of seen) {
    if (!covered.has(channel)) ordered.push(fallbackView(channel));
  }
  ordered.sort(byOrder);

  const claimed = new Set<string>();
  const views: LogView[] = [];
  for (const view of ordered) {
    const channels = view.channels.filter((c) => !claimed.has(c));
    for (const channel of channels) claimed.add(channel);
    if (channels.length > 0) views.push({ ...view, channels });
  }
  return views;
}

function byOrder(a: LogView, b: LogView): number {
  return a.order - b.order || a.id.localeCompare(b.id);
}

export type Frame = { channel: string; data: Uint8Array };

type Sink = (frame: Frame) => void;

/**
 * The one connection's messages, split by channel and handed to whichever view
 * renders each. Recent frames are retained per channel and replayed to a sink
 * on subscribe.
 */
export class ChannelBus {
  private history = new Map<string, { frames: Frame[]; bytes: number }>();
  private sinks = new Map<string, Set<Sink>>();

  push(frame: Frame): void {
    const retained = this.history.get(frame.channel) ?? {
      frames: [],
      bytes: 0,
    };
    retained.frames.push(frame);
    retained.bytes += frame.data.length;
    while (retained.bytes > RETAINED_BYTES_PER_CHANNEL) {
      const dropped = retained.frames.shift();
      if (dropped === undefined) break;
      retained.bytes -= dropped.data.length;
    }
    this.history.set(frame.channel, retained);

    for (const sink of this.sinks.get(frame.channel) ?? []) sink(frame);
  }

  subscribe(channel: string, sink: Sink): () => void {
    for (const frame of this.history.get(channel)?.frames ?? []) sink(frame);

    const sinks = this.sinks.get(channel) ?? new Set<Sink>();
    sinks.add(sink);
    this.sinks.set(channel, sinks);
    return () => {
      sinks.delete(sink);
    };
  }
}
