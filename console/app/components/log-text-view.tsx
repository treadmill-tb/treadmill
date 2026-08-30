import { useEffect, useLayoutEffect, useRef, useState } from "react";

import {
  LINE_CAP,
  LineSplitter,
  type ChannelBus,
  type LogView,
} from "./log-stream";

/** One rendered line, tagged with the channel it came from. */
type Line = { channel: string; text: string };

/** One event of a `jsonl` channel; every field is optional, since a line that
 * does not parse this way is shown as plain text instead. */
type LogEvent = {
  ts?: unknown;
  level?: unknown;
  target?: unknown;
  message?: unknown;
  fields?: unknown;
};

const LEVEL_BADGE: Record<string, string> = {
  ERROR: "badge danger",
  WARN: "badge warn",
  INFO: "badge ok",
};

/**
 * A view rendered as a scrolling list of lines: the supervisor's own events,
 * and any console channel whose bytes are not meant for a terminal.
 *
 * Channels are decoded and split into lines independently — a frame boundary
 * falls anywhere, so each carries its own partial line until the newline that
 * ends it arrives.
 */
export function LogTextView({
  view,
  bus,
  active,
}: {
  view: LogView;
  bus: ChannelBus;
  active: boolean;
}) {
  const [lines, setLines] = useState<Line[]>([]);
  const pendingRef = useRef<Line[]>([]);
  const flushRef = useRef<number | null>(null);
  const boxRef = useRef<HTMLDivElement | null>(null);
  const atBottomRef = useRef(true);

  // The view's channel set is part of its key, so an instance only ever
  // renders one set and its state never needs resetting.
  const channels = view.channels.join(" ");
  useEffect(() => {
    // One splitter per channel: a frame boundary falls anywhere, so each
    // channel carries its own partial line and decoder state across frames.
    const splitters = new Map<string, LineSplitter>();

    const flush = () => {
      flushRef.current = null;
      const pending = pendingRef.current;
      if (pending.length === 0) return;
      pendingRef.current = [];
      setLines((prev) => {
        const next = prev.concat(pending);
        return next.length > LINE_CAP
          ? next.slice(next.length - LINE_CAP)
          : next;
      });
    };

    const unsubscribe = channels.split(" ").map((channel) =>
      bus.subscribe(channel, (frame) => {
        let splitter = splitters.get(channel);
        if (splitter === undefined) {
          splitter = new LineSplitter();
          splitters.set(channel, splitter);
        }
        for (const line of splitter.push(frame.data)) {
          pendingRef.current.push({ channel, text: line.replace(/\r/g, "") });
        }
        // Coalesce a burst of frames into one render.
        if (flushRef.current === null) {
          flushRef.current = requestAnimationFrame(flush);
        }
      }),
    );

    return () => {
      for (const unsub of unsubscribe) unsub();
      if (flushRef.current !== null) cancelAnimationFrame(flushRef.current);
      flushRef.current = null;
    };
  }, [bus, channels]);

  // Follow the tail unless the reader has scrolled away from it. A hidden tab
  // measures as scrolled to the top, so leave its position alone.
  useLayoutEffect(() => {
    const box = boxRef.current;
    if (box !== null && active && atBottomRef.current) {
      box.scrollTop = box.scrollHeight;
    }
  }, [lines, active]);

  const showTag = view.channels.length > 1;
  return (
    <div
      ref={boxRef}
      className="log-text"
      onScroll={() => {
        const box = boxRef.current;
        if (box !== null) {
          atBottomRef.current =
            box.scrollHeight - box.scrollTop - box.clientHeight < 4;
        }
      }}
    >
      {lines.map((line, i) => (
        <div
          // Lines have no identity of their own, and the list only ever grows
          // at the end or drops from the front.
          key={i}
          className="log-line"
          data-channel={line.channel}
        >
          {showTag && <span className="log-tag">{line.channel}</span>}
          {view.format === "jsonl" ? renderEvent(line.text) : line.text}
        </div>
      ))}
    </div>
  );
}

/** One JSONL line as a level badge, a timestamp and the event's fields —
 * falling back to the raw text when it does not parse as one. */
function renderEvent(text: string) {
  let event: LogEvent;
  try {
    const parsed: unknown = JSON.parse(text);
    if (typeof parsed !== "object" || parsed === null) return text;
    event = parsed;
  } catch {
    return text;
  }

  const level = typeof event.level === "string" ? event.level : "";
  const fields =
    typeof event.fields === "object" && event.fields !== null
      ? Object.entries(event.fields)
      : [];

  return (
    <>
      <span className="log-ts">{formatTs(event.ts)}</span>
      <span className={LEVEL_BADGE[level] ?? "badge"}>{level || "?"}</span>{" "}
      {typeof event.message === "string" ? event.message : ""}
      {fields.map(([key, value]) => (
        <span key={key} className="log-field">
          {" "}
          {key}={String(value)}
        </span>
      ))}
    </>
  );
}

/** The event's wall-clock time, in the reader's zone. */
function formatTs(ts: unknown): string {
  if (typeof ts !== "string") return "";
  const at = new Date(ts);
  if (Number.isNaN(at.getTime())) return "";
  return at.toLocaleTimeString(undefined, { hour12: false });
}
