import { DeliverPolicy, jetstream } from "@nats-io/jetstream";
import type { OrderedConsumerOptions } from "@nats-io/jetstream";
import {
  jwtAuthenticator,
  wsconnect,
  type NatsConnection,
} from "@nats-io/nats-core";
import { useEffect, useMemo, useState } from "react";

import { client } from "../api/client";
import { LogTerminalView } from "./log-terminal-view";
import { LogTextView } from "./log-text-view";
import {
  ChannelBus,
  parseManifestLine,
  resolveViews,
  type LogView,
} from "./log-stream";

type Status =
  | { kind: "connecting" }
  | { kind: "waiting" }
  | { kind: "replaying" }
  | { kind: "live" }
  | { kind: "reconnecting" }
  | { kind: "disabled" }
  | { kind: "no-websocket" }
  | { kind: "error"; message: string };

const RETRY_DELAY_MS = 3_000;

/** Default bound on replayed history. ~1 MiB roughly matches the terminal's
 * scrollback; anything beyond the scrollback is discarded on arrival
 * anyway. */
export const DEFAULT_REPLAY_BYTES = 1 << 20;

/** Upper clamp for the `?replay=` override, protecting the browser tab. */
const MAX_REPLAY_BYTES = 64 << 20;

/** Parse the `?replay=` override: bytes, optionally with a binary `k`/`M`
 * suffix (`?replay=256k`); `0` disables replay (live tail only). Invalid
 * values fall back to the default; the result is clamped to the maximum. */
export function parseReplayBytes(raw: string | null): number {
  if (raw === null) return DEFAULT_REPLAY_BYTES;
  const m = /^(\d+)([kM]?)$/.exec(raw.trim());
  if (m === null) return DEFAULT_REPLAY_BYTES;
  const unit = m[2] === "k" ? 1 << 10 : m[2] === "M" ? 1 << 20 : 1;
  return Math.min(Number(m[1]) * unit, MAX_REPLAY_BYTES);
}

function bytesLabel(n: number): string {
  if (n >= 1 << 20) return `${+(n / (1 << 20)).toFixed(1)} MiB`;
  if (n >= 1 << 10) return `${+(n / (1 << 10)).toFixed(1)} KiB`;
  return `${n} B`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** The channel a message belongs to: the last token of `logs.<job-id>.<channel>`. */
function channelOf(subject: string): string {
  return subject.slice(subject.lastIndexOf(".") + 1);
}

/** The `meta` subject within a job's `logs.<job-id>.>` scope. */
function metaSubject(scope: string): string {
  return `${scope.slice(0, scope.lastIndexOf("."))}.meta`;
}

/**
 * A job's log stream: replays up to `replayBytes` of stored history from the
 * job's JetStream stream, then follows live. A single ordered consumer serves
 * both the backlog and the tail of every channel, so nothing is lost in
 * between and the ordering between channels is preserved; across reconnects
 * it resumes after the last sequence already delivered. A second, small
 * consumer reads the job's `meta` channel in full, since the main consumer's
 * replay is byte-bounded and may start past the early declarations.
 *
 * Credentials are re-requested on every (re)connect, satisfying the
 * expires_in_secs contract.
 */
export function JobLog({
  jobId,
  replayBytes = DEFAULT_REPLAY_BYTES,
  canSendInput = false,
}: {
  jobId: string;
  replayBytes?: number;
  canSendInput?: boolean;
}) {
  const [status, setStatus] = useState<Status>({ kind: "connecting" });
  const [truncated, setTruncated] = useState(false);
  const [declared, setDeclared] = useState<Map<string, LogView>>(new Map());
  const [seen, setSeen] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const bus = useMemo(() => new ChannelBus(), []);

  useEffect(() => {
    let cancelled = false;
    let nc: NatsConnection | null = null;
    const seenChannels = new Set<string>();

    async function run() {
      let first = true;
      // Resume cursor across reconnects: the last stream sequence delivered
      // (0 = nothing yet, do the initial replay).
      let lastSeq = 0;
      while (!cancelled) {
        if (!first) {
          setStatus({ kind: "reconnecting" });
          await sleep(RETRY_DELAY_MS);
          if (cancelled) return;
        }
        first = false;

        let creds;
        try {
          creds = await client.POST("/jobs/{id}/nats-log-token", {
            params: { path: { id: jobId } },
          });
        } catch {
          continue;
        }
        if (creds.response.status === 503) {
          setStatus({ kind: "disabled" });
          return;
        }
        if (creds.data === undefined) {
          setStatus({
            kind: "error",
            message: `Fetching log credentials failed (HTTP ${creds.response.status}).`,
          });
          return;
        }

        // Browsers can only speak the WebSocket protocol; a deployment that
        // exposes no WebSocket listener (`websocket_url` absent) cannot serve
        // the console, even though log streaming is otherwise enabled. This is
        // distinct from the feature being off entirely (the 503 above).
        const websocketUrl = creds.data.websocket_url;
        if (websocketUrl === undefined || websocketUrl === null) {
          setStatus({ kind: "no-websocket" });
          return;
        }

        try {
          // Reconnection is handled by this loop, not the client: each new
          // connection needs freshly minted credentials.
          nc = await wsconnect({
            servers: [websocketUrl],
            authenticator: jwtAuthenticator(creds.data.token),
            // The token's subscribe permission covers only inboxes under
            // this per-job prefix, not the account-default `_INBOX.>`.
            inboxPrefix: creds.data.inbox_prefix,
            reconnect: false,
          });
        } catch {
          continue;
        }
        if (cancelled) {
          void nc.close();
          return;
        }

        // One ordered consumer serves both the stored backlog and the live
        // tail: messages arriving after the STREAM.INFO snapshot below have
        // higher sequences and are delivered in order — there is no seam to
        // lose messages in.
        let messages;
        let manifest;
        let replayEnd = 0; // last stored sequence at setup (backlog <= it)
        let cut = false;
        try {
          const domain = creds.data.jetstream_domain ?? undefined;
          const js = jetstream(nc, domain === undefined ? {} : { domain });
          let opts: Partial<OrderedConsumerOptions>;
          if (lastSeq > 0) {
            // Reconnect: resume exactly after what has been delivered — no
            // re-replay, and messages published during the outage arrive
            // rather than being lost.
            opts = {
              deliver_policy: DeliverPolicy.StartSequence,
              opt_start_seq: lastSeq + 1,
            };
          } else if (replayBytes === 0) {
            opts = { deliver_policy: DeliverPolicy.New };
          } else {
            // JetStream has no "last N bytes" deliver policy: estimate a
            // start sequence from the stream's byte/message counts. `bytes`
            // includes per-message overhead, so this undershoots the cap a
            // little — it is a soft bound protecting the browser, enforced
            // here and not by the token.
            const stream = await js.streams.get(creds.data.stream);
            const state = (await stream.info(true)).state;
            if (state.bytes <= replayBytes) {
              opts = { deliver_policy: DeliverPolicy.All };
            } else {
              const avg = state.bytes / state.messages;
              const start = Math.max(
                state.first_seq,
                state.last_seq - Math.ceil(replayBytes / avg) + 1,
              );
              opts = {
                deliver_policy: DeliverPolicy.StartSequence,
                opt_start_seq: start,
              };
              cut = start > state.first_seq;
            }
            replayEnd = state.last_seq;
          }
          const consumer = await js.consumers.get(creds.data.stream, opts);
          messages = await consumer.consume();

          // Declarations are cumulative and can arrive at any point in the
          // job's life, so this one reads the channel whole and stays open.
          // Re-reading it on a reconnect is harmless: applying a declaration
          // twice is applying it once.
          const metaConsumer = await js.consumers.get(creds.data.stream, {
            deliver_policy: DeliverPolicy.All,
            filter_subjects: metaSubject(creds.data.subject),
          });
          manifest = await metaConsumer.consume();
        } catch {
          // Most likely the stream does not exist yet (the job has not been
          // dispatched); wait and retry. Other failures retry the same way.
          if (lastSeq === 0) setStatus({ kind: "waiting" });
          await nc.close();
          nc = null;
          continue;
        }
        if (cancelled) {
          void nc.close();
          return;
        }

        let caughtUp = lastSeq > 0 || replayEnd === 0;
        setStatus(caughtUp ? { kind: "live" } : { kind: "replaying" });
        if (cut) setTruncated(true);
        // Truncation cosmetics: the cut lands on a message boundary, but that
        // boundary is an arbitrary point in a channel's raw byte stream.
        // Skipping each channel's first frame past its first newline avoids
        // starting mid-line or mid-escape-sequence in the common,
        // line-structured case.
        const skipToNewline = new Set<string>();

        void (async () => {
          try {
            for await (const msg of manifest) {
              const declarations = parseManifestLine(
                new TextDecoder().decode(msg.data),
              );
              if (declarations === null) continue;
              setDeclared((prev) => {
                const next = new Map(prev);
                for (const view of declarations) next.set(view.id, view);
                return next;
              });
            }
          } catch {
            // Dies with the connection, which the outer loop already awaits.
          }
        })();

        void (async () => {
          try {
            for await (const msg of messages) {
              const channel = channelOf(msg.subject);
              let data = msg.data;
              if (cut && !skipToNewline.has(channel)) {
                skipToNewline.add(channel);
                const nl = data.indexOf(0x0a);
                if (nl !== -1) data = data.subarray(nl + 1);
              }
              bus.push({ channel, data });
              if (!seenChannels.has(channel)) {
                seenChannels.add(channel);
                setSeen([...seenChannels]);
              }
              lastSeq = msg.info.streamSequence;
              if (!caughtUp && lastSeq >= replayEnd) {
                caughtUp = true;
                setStatus({ kind: "live" });
              }
            }
          } catch {
            // The iterator dies with the connection; the outer loop's
            // `nc.closed()` wake-up handles the retry.
          }
        })();
        await nc.closed();
        nc = null;
      }
    }
    void run();

    return () => {
      cancelled = true;
      void nc?.close();
    };
  }, [jobId, replayBytes, bus]);

  const views = useMemo(
    () => resolveViews(declared.values(), seen),
    [declared, seen],
  );
  const activeId =
    views.find((view) => view.id === selected)?.id ??
    views.find((view) => view.default)?.id ??
    views[0]?.id;

  if (status.kind === "disabled") {
    return null;
  }

  if (status.kind === "no-websocket") {
    return (
      <section>
        <h2>Logs</h2>
        <p className="error">
          Log streaming is enabled, but this deployment does not expose a NATS
          WebSocket endpoint, so logs cannot be tailed from the browser.
        </p>
      </section>
    );
  }

  return (
    <section>
      <h2>
        Logs {status.kind === "live" && <span className="badge ok">live</span>}
        {status.kind === "replaying" && (
          <span className="badge ok">replaying</span>
        )}
        {(status.kind === "connecting" ||
          status.kind === "reconnecting" ||
          status.kind === "waiting") && (
          <span className="badge warn">
            {status.kind === "waiting" ? "waiting for logs" : status.kind}
          </span>
        )}
      </h2>
      {status.kind === "error" ? (
        <p className="error">{status.message}</p>
      ) : (
        <>
          <div className="log-tabs" role="tablist">
            {views.map((view) => (
              <button
                key={view.id}
                role="tab"
                aria-selected={view.id === activeId}
                onClick={() => setSelected(view.id)}
              >
                {view.label}
              </button>
            ))}
          </div>
          {views.map((view) => (
            // Views stay mounted and inactive ones are hidden, so switching
            // tabs never replays a buffer into a freshly mounted component.
            // The channels are part of the key: a view that gains one starts
            // over, and the bus replays what it missed.
            <div
              key={`${view.id} ${view.channels.join(" ")}`}
              hidden={view.id !== activeId}
            >
              {view.render === "terminal" ? (
                <LogTerminalView
                  view={view}
                  bus={bus}
                  jobId={jobId}
                  canSendInput={canSendInput}
                  active={view.id === activeId}
                />
              ) : (
                <LogTextView
                  view={view}
                  bus={bus}
                  active={view.id === activeId}
                />
              )}
            </div>
          ))}
          <p className="muted">
            {truncated && `Earlier output omitted. `}
            {replayBytes === 0
              ? "Live tail only (history replay disabled by ?replay=0)."
              : `Replays up to ~${bytesLabel(replayBytes)} of stored history, then follows live (override with ?replay=).`}
          </p>
        </>
      )}
    </section>
  );
}
