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
  LineSplitter,
  META_CHANNEL,
  parseManifestLine,
  resolveViews,
  type LogView,
} from "./log-stream";

type Status =
  | { kind: "connecting" }
  /** Connected, but the job's stream does not exist yet. */
  | { kind: "waiting" }
  | { kind: "replaying" }
  | { kind: "live" }
  /** Between attempts, carrying why the last one ended. */
  | { kind: "retrying"; reason: string }
  | { kind: "disabled" }
  | { kind: "no-websocket" }
  | { kind: "error"; message: string };

/** Backoff between connection attempts, doubling up to the cap. */
const FIRST_RETRY_MS = 1_000;
const MAX_RETRY_MS = 30_000;

/** Bound on the NATS handshake. `wsconnect` resolves once the server's `INFO`
 * arrives, and a WebSocket that opens but never speaks — an upgrade a proxy
 * accepted and then stalled — would otherwise leave the loop waiting. */
const CONNECT_TIMEOUT_MS = 10_000;

/** How long the manifest gets to catch up before the log consumer starts.
 * Only ever cosmetic: a declaration arriving later still claims its tab. */
const META_PRIME_TIMEOUT_MS = 5_000;

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

/** What went wrong, in as many words as the thrower gave us. */
function reasonOf(e: unknown): string {
  if (e instanceof Error && e.message !== "") return e.message;
  const described = String(e);
  return described === "" ? "unknown error" : described;
}

/** A promise plus the handle to settle it. */
function signal(): { armed: Promise<void>; fire: () => void } {
  let fire: () => void = () => undefined;
  const armed = new Promise<void>((resolve) => {
    fire = resolve;
  });
  return { armed, fire };
}

/** The channel a message belongs to: the last token of `logs.<job-id>.<channel>`. */
function channelOf(subject: string): string {
  return subject.slice(subject.lastIndexOf(".") + 1);
}

/** The `meta` subject within a job's `logs.<job-id>.>` scope. */
function metaSubject(scope: string): string {
  return `${scope.slice(0, scope.lastIndexOf("."))}.${META_CHANNEL}`;
}

/**
 * A job's log stream: replays up to `replayBytes` of stored history from the
 * job's JetStream stream, then follows live.
 *
 * Each connection reads the job's `meta` channel first, on its own filtered
 * consumer, and waits for the stored manifest to drain before the log consumer
 * starts — so the tabs are the declared ones from the first byte rather than
 * fallbacks that get replaced underneath the reader. That consumer then stays
 * open: declarations are cumulative and keep arriving as the job announces
 * channels. The manifest is bounded work, but it is never allowed to hold up
 * the logs: past `META_PRIME_TIMEOUT_MS` the log consumer starts regardless.
 *
 * A single ordered consumer then serves both the backlog and the tail of every
 * channel, so nothing is lost in between and the ordering between channels is
 * preserved; across reconnects it resumes after the last sequence already
 * delivered. `meta` reaches it too — it is one of the job's subjects — and is
 * skipped there, since the manifest consumer owns it.
 *
 * Credentials are re-requested on every (re)connect, satisfying the
 * `expires_in_secs` contract. Attempts back off, and the reason the last one
 * ended is kept and displayed: a deployment whose NATS endpoint a browser
 * cannot reach is a configuration problem, and saying so beats retrying in
 * silence.
 */
export function JobLog({
  jobId,
  dispatched,
  replayBytes = DEFAULT_REPLAY_BYTES,
  canSendInput = false,
}: {
  jobId: string;
  /** Whether the job has been placed on a host. The switchboard creates the
   * stream as it dispatches, so there is nothing to connect to before that. */
  dispatched: boolean;
  replayBytes?: number;
  canSendInput?: boolean;
}) {
  const [status, setStatus] = useState<Status>({ kind: "connecting" });
  const [truncated, setTruncated] = useState(false);
  const [declared, setDeclared] = useState<Map<string, LogView>>(new Map());
  const [seen, setSeen] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  // Buffers belong to one connection's demux. `JobLog` is keyed by job, so a
  // different job mounts a fresh component and a fresh bus.
  const bus = useMemo(() => new ChannelBus(), []);

  useEffect(() => {
    // Nothing to connect to yet; the render below says so. `JobLog` is keyed
    // by job and replay bound, so every other state here starts fresh with the
    // component rather than needing a reset.
    if (!dispatched) return;

    let cancelled = false;
    let nc: NatsConnection | null = null;
    const seenChannels = new Set<string>();
    // Resume cursor across reconnects: the last stream sequence delivered
    // (0 = nothing yet, do the initial replay).
    let lastSeq = 0;
    // Whether the current session got as far as consuming, which is what
    // separates a flapping connection from one that never comes up.
    let streamed = false;

    /**
     * One connection, from credentials to close. Returns why to retry, or
     * `null` when the failure is permanent and the loop must stop (having set
     * the status that says so).
     */
    async function session(): Promise<string | null> {
      let creds;
      try {
        creds = await client.POST("/jobs/{id}/nats-log-token", {
          params: { path: { id: jobId } },
        });
      } catch (e) {
        return `requesting log credentials: ${reasonOf(e)}`;
      }
      if (cancelled) return null;
      if (creds.response.status === 503) {
        setStatus({ kind: "disabled" });
        return null;
      }
      if (creds.data === undefined) {
        setStatus({
          kind: "error",
          message: `Fetching log credentials failed (HTTP ${creds.response.status}).`,
        });
        return null;
      }

      // Browsers can only speak the WebSocket protocol; a deployment that
      // exposes no WebSocket listener (`websocket_url` absent) cannot serve
      // the console, even though log streaming is otherwise enabled. This is
      // distinct from the feature being off entirely (the 503 above).
      const websocketUrl = creds.data.websocket_url;
      if (websocketUrl === undefined || websocketUrl === null) {
        setStatus({ kind: "no-websocket" });
        return null;
      }

      // Reconnection is handled by this loop, not the client: each new
      // connection needs freshly minted credentials.
      const pending = wsconnect({
        servers: [websocketUrl],
        authenticator: jwtAuthenticator(creds.data.token),
        // The token's subscribe permission covers only inboxes under this
        // per-job prefix, not the account-default `_INBOX.>`.
        inboxPrefix: creds.data.inbox_prefix,
        reconnect: false,
      });
      let conn: NatsConnection;
      try {
        const raced = await Promise.race([
          pending,
          sleep(CONNECT_TIMEOUT_MS).then(() => null),
        ]);
        if (raced === null) {
          // Don't leak a connection that lands after we gave up on it.
          void pending.then((late) => void late.close()).catch(() => undefined);
          return `no reply from ${websocketUrl} within ${CONNECT_TIMEOUT_MS / 1000}s`;
        }
        conn = raced;
      } catch (e) {
        return `connecting to ${websocketUrl}: ${reasonOf(e)}`;
      }
      nc = conn;
      if (cancelled) {
        void conn.close();
        return null;
      }

      try {
        return await stream(conn, creds.data);
      } catch (e) {
        void conn.close();
        await conn.closed();
        nc = null;
        return reasonOf(e);
      }
    }

    /** Set up both consumers on `conn` and pump them until it closes. */
    async function stream(
      conn: NatsConnection,
      creds: {
        stream: string;
        subject: string;
        jetstream_domain?: string | null;
      },
    ): Promise<string | null> {
      const domain = creds.jetstream_domain ?? undefined;
      const js = jetstream(conn, domain === undefined ? {} : { domain });

      // The stream is created as the job is dispatched, so its absence is the
      // ordinary "the supervisor has not started publishing yet" case rather
      // than a fault. Everything below needs the snapshot anyway.
      let state;
      try {
        const stored = await js.streams.get(creds.stream);
        state = (await stored.info(true)).state;
      } catch (e) {
        if (lastSeq === 0) setStatus({ kind: "waiting" });
        void conn.close();
        await conn.closed();
        nc = null;
        return `waiting for the job's log stream: ${reasonOf(e)}`;
      }
      if (cancelled) {
        void conn.close();
        return null;
      }

      // Read the manifest whole and stay open. Re-reading it on a reconnect is
      // harmless: applying a declaration twice is applying it once.
      const metaConsumer = await js.consumers.get(creds.stream, {
        deliver_policy: DeliverPolicy.All,
        filter_subjects: metaSubject(creds.subject),
      });
      // `num_pending` as of the consumer's creation: how much of the manifest
      // is already stored, and so how much to wait for. Cached — the count
      // came back with the consumer itself, no second round trip — and read
      // before consuming, which may reset the consumer underneath it.
      let backlog = (await metaConsumer.info(true)).num_pending;
      const manifest = await metaConsumer.consume();
      const primed = signal();

      void (async () => {
        // The publisher batches reads into frames, so one message can carry
        // several declarations, or half of one. Splitting them out is what
        // keeps a manifest that arrives all at once from being dropped whole.
        const lines = new LineSplitter();
        try {
          for await (const msg of manifest) {
            if (cancelled) return;
            for (const line of lines.push(msg.data)) {
              const declarations = parseManifestLine(line);
              if (declarations === null) continue;
              setDeclared((prev) => {
                const next = new Map(prev);
                for (const view of declarations) next.set(view.id, view);
                return next;
              });
            }
            backlog -= 1;
            if (backlog <= 0 || msg.info.pending === 0) primed.fire();
          }
        } catch {
          // Dies with the connection, which this session already awaits.
        } finally {
          primed.fire();
        }
      })();

      if (backlog <= 0) primed.fire();
      await Promise.race([primed.armed, sleep(META_PRIME_TIMEOUT_MS)]);
      if (cancelled) {
        void conn.close();
        return null;
      }

      // One ordered consumer serves both the stored backlog and the live tail:
      // messages arriving after the snapshot above have higher sequences and
      // are delivered in order — there is no seam to lose messages in.
      let replayEnd = 0; // last stored sequence at setup (backlog <= it)
      let cut = false;
      let opts: Partial<OrderedConsumerOptions>;
      if (lastSeq > 0) {
        // Reconnect: resume exactly after what has been delivered — no
        // re-replay, and messages published during the outage arrive rather
        // than being lost.
        opts = {
          deliver_policy: DeliverPolicy.StartSequence,
          opt_start_seq: lastSeq + 1,
        };
      } else if (replayBytes === 0) {
        opts = { deliver_policy: DeliverPolicy.New };
      } else {
        // JetStream has no "last N bytes" deliver policy: estimate a start
        // sequence from the stream's byte/message counts. `bytes` includes
        // per-message overhead, so this undershoots the cap a little — it is a
        // soft bound protecting the browser, enforced here and not by the
        // token.
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
      const consumer = await js.consumers.get(creds.stream, opts);
      const messages = await consumer.consume();
      if (cancelled) {
        void conn.close();
        return null;
      }

      streamed = true;
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
          for await (const msg of messages) {
            if (cancelled) return;
            lastSeq = msg.info.streamSequence;
            if (!caughtUp && lastSeq >= replayEnd) {
              caughtUp = true;
              setStatus({ kind: "live" });
            }
            // The manifest consumer owns this channel: its bytes declare the
            // tabs rather than filling one.
            const channel = channelOf(msg.subject);
            if (channel === META_CHANNEL) continue;

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
          }
        } catch {
          // The iterator dies with the connection; the wait below handles the
          // retry.
        }
      })();

      await conn.closed();
      nc = null;
      return "the log connection closed";
    }

    async function run() {
      let attempt = 0;
      while (!cancelled) {
        streamed = false;
        const reason = await session();
        if (cancelled || reason === null) return;
        // A session that got as far as streaming starts the backoff over; one
        // that never came up keeps doubling, up to the cap.
        if (streamed) attempt = 0;
        const delay = Math.min(FIRST_RETRY_MS * 2 ** attempt, MAX_RETRY_MS);
        attempt += 1;
        setStatus({ kind: "retrying", reason });
        await sleep(delay);
      }
    }
    void run();

    return () => {
      cancelled = true;
      void nc?.close();
    };
  }, [jobId, dispatched, replayBytes, bus]);

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

  if (!dispatched) {
    return (
      <section>
        <h2>Logs</h2>
        <p className="muted">
          The job has not been placed on a host yet; its log stream starts when
          it is dispatched.
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
          status.kind === "retrying" ||
          status.kind === "waiting") && (
          <span className="badge warn">
            {status.kind === "waiting"
              ? "waiting for logs"
              : status.kind === "retrying"
                ? "reconnecting"
                : status.kind}
          </span>
        )}
      </h2>
      {status.kind === "error" ? (
        <p className="error">{status.message}</p>
      ) : (
        <>
          {status.kind === "retrying" && (
            <p className="muted">Retrying: {status.reason}.</p>
          )}
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
