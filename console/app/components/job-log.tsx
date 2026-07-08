import { DeliverPolicy, jetstream } from "@nats-io/jetstream";
import type { OrderedConsumerOptions } from "@nats-io/jetstream";
import {
  jwtAuthenticator,
  wsconnect,
  type NatsConnection,
} from "@nats-io/nats-core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { useEffect, useRef, useState } from "react";

import { client } from "../api/client";

import "@xterm/xterm/css/xterm.css";

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
 * 10k-line scrollback; anything beyond the scrollback is discarded on
 * arrival anyway. */
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

/**
 * A job's console channels over NATS: replays up to `replayBytes` of stored
 * history from the job's JetStream stream, then follows live
 * (doc/log-streaming-plan.md §4b). A single ordered consumer serves both the
 * backlog and the tail, so nothing is lost in between; across reconnects it
 * resumes after the last sequence already written to the terminal.
 * Credentials are re-requested on every (re)connect, satisfying the
 * expires_in_secs contract.
 */
export function JobLog({
  jobId,
  replayBytes = DEFAULT_REPLAY_BYTES,
}: {
  jobId: string;
  replayBytes?: number;
}) {
  const mountRef = useRef<HTMLDivElement | null>(null);
  const [status, setStatus] = useState<Status>({ kind: "connecting" });

  useEffect(() => {
    const mount = mountRef.current;
    if (mount === null) return;

    let cancelled = false;
    let nc: NatsConnection | null = null;

    const term = new Terminal({
      disableStdin: true,
      convertEol: false,
      scrollback: 10_000,
      fontSize: 12,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(mount);
    fit.fit();
    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);

    async function run() {
      let first = true;
      // Resume cursor across reconnects: the last stream sequence written to
      // the terminal (0 = nothing delivered yet, do the initial replay).
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
        let replayEnd = 0; // last stored sequence at setup (backlog <= it)
        let truncated = false;
        try {
          const domain = creds.data.jetstream_domain ?? undefined;
          const js = jetstream(nc, domain === undefined ? {} : { domain });
          let opts: Partial<OrderedConsumerOptions>;
          if (lastSeq > 0) {
            // Reconnect: resume exactly after what the terminal has seen —
            // no re-replay, and messages published during the outage are
            // delivered rather than lost.
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
              truncated = start > state.first_seq;
            }
            replayEnd = state.last_seq;
          }
          const consumer = await js.consumers.get(creds.data.stream, opts);
          messages = await consumer.consume();
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
        if (truncated) {
          term.write(
            `\x1b[0m\x1b[2m--- earlier output omitted (replaying the last ~${bytesLabel(replayBytes)}) ---\x1b[0m\r\n`,
          );
        }
        // Truncation cosmetics: the cut lands on a message boundary, but
        // that boundary is an arbitrary point in the raw byte stream.
        // Skipping just past the first newline avoids starting mid-line /
        // mid-escape-sequence in the common, line-structured case; a
        // residual truncated sequence garbles at most briefly until xterm's
        // parser resyncs.
        let skipToNewline = truncated;
        void (async () => {
          try {
            for await (const msg of messages) {
              let data = msg.data;
              if (skipToNewline) {
                skipToNewline = false;
                const nl = data.indexOf(0x0a);
                if (nl !== -1) data = data.subarray(nl + 1);
              }
              term.write(data);
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
      window.removeEventListener("resize", onResize);
      void nc?.close();
      term.dispose();
    };
  }, [jobId, replayBytes]);

  if (status.kind === "disabled") {
    return null;
  }

  if (status.kind === "no-websocket") {
    return (
      <section>
        <h2>Console log</h2>
        <p className="error">
          Log streaming is enabled, but this deployment does not expose a NATS
          WebSocket endpoint, so console logs cannot be tailed from the browser.
        </p>
      </section>
    );
  }

  return (
    <section>
      <h2>
        Console log{" "}
        {status.kind === "live" && <span className="badge ok">live</span>}
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
          <div ref={mountRef} className="job-log-term" />
          <p className="muted">
            {replayBytes === 0
              ? "Live tail only (history replay disabled by ?replay=0)."
              : `Replays up to ~${bytesLabel(replayBytes)} of stored history, then follows live (override with ?replay=).`}
          </p>
        </>
      )}
    </section>
  );
}
