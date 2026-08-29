import {
  jwtAuthenticator,
  wsconnect,
  type NatsConnection,
} from "@nats-io/nats-core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { useEffect, useRef, useState } from "react";

import { client } from "../api/client";
import { LINE_CAP, type ChannelBus, type LogView } from "./log-stream";

import "@xterm/xterm/css/xterm.css";

type InputStatus =
  | { kind: "off" }
  | { kind: "connecting" }
  | { kind: "on" }
  | { kind: "unavailable" }
  | { kind: "error"; message: string };

const RETRY_DELAY_MS = 3_000;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * A view rendered as a terminal: its channels' bytes are whatever the workload
 * wrote, escape sequences included, so xterm interprets them.
 *
 * A view declaring `input` offers to start an input session: keystrokes are
 * published to the job's console-input subject over a second, publish-only
 * connection (every mint of its token is audited server-side, as is every byte
 * sent). There is no local echo — feedback arrives through the log channel
 * like any other console output.
 */
export function LogTerminalView({
  view,
  bus,
  jobId,
  canSendInput,
  active,
}: {
  view: LogView;
  bus: ChannelBus;
  jobId: string;
  canSendInput: boolean;
  active: boolean;
}) {
  const mountRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const [inputEnabled, setInputEnabled] = useState(false);
  const [inputStatus, setInputStatus] = useState<InputStatus>({ kind: "off" });
  // The live input connection keystrokes are published over, if any.
  const inputConnRef = useRef<{ nc: NatsConnection; subject: string } | null>(
    null,
  );

  const channels = view.channels.join(" ");
  useEffect(() => {
    const mount = mountRef.current;
    if (mount === null) return;

    const term = new Terminal({
      disableStdin: true,
      convertEol: false,
      scrollback: LINE_CAP,
      fontSize: 12,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(mount);
    fit.fit();
    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);
    termRef.current = term;
    fitRef.current = fit;

    // Keystrokes go to the input connection when one is live (xterm emits no
    // input while `disableStdin` is set, which is whenever there is none).
    const encoder = new TextEncoder();
    term.onData((data) => {
      const conn = inputConnRef.current;
      if (conn !== null) conn.nc.publish(conn.subject, encoder.encode(data));
    });

    const unsubscribe = channels
      .split(" ")
      .map((channel) =>
        bus.subscribe(channel, (frame) => term.write(frame.data)),
      );

    return () => {
      for (const unsub of unsubscribe) unsub();
      window.removeEventListener("resize", onResize);
      termRef.current = null;
      fitRef.current = null;
      term.dispose();
    };
  }, [bus, channels]);

  // xterm measures wrong while the tab is hidden, so refit on the way in.
  useEffect(() => {
    if (active) fitRef.current?.fit();
  }, [active]);

  // Input session: its own connection (parallel to the read loop), living
  // only while enabled. Each (re)connect mints a fresh token — and each mint
  // is audited — so the loop re-requests credentials whenever the connection
  // closes while input is still enabled.
  useEffect(() => {
    if (!inputEnabled) return;

    let cancelled = false;
    let nc: NatsConnection | null = null;

    async function run() {
      let first = true;
      while (!cancelled) {
        if (!first) {
          await sleep(RETRY_DELAY_MS);
          if (cancelled) return;
        }
        first = false;
        setInputStatus({ kind: "connecting" });

        let creds;
        try {
          creds = await client.POST("/jobs/{id}/nats-console-input-token", {
            params: { path: { id: jobId } },
          });
        } catch {
          continue;
        }
        if (creds.response.status === 403) {
          // E.g. `manage` was revoked mid-session; surface it and flip off.
          setInputStatus({
            kind: "error",
            message: "You are not authorized to send console input.",
          });
          setInputEnabled(false);
          return;
        }
        if (creds.response.status === 503) {
          setInputStatus({ kind: "unavailable" });
          setInputEnabled(false);
          return;
        }
        if (creds.data === undefined) {
          setInputStatus({
            kind: "error",
            message: `Fetching input credentials failed (HTTP ${creds.response.status}).`,
          });
          setInputEnabled(false);
          return;
        }
        const websocketUrl = creds.data.websocket_url;
        if (websocketUrl === undefined || websocketUrl === null) {
          setInputStatus({ kind: "unavailable" });
          setInputEnabled(false);
          return;
        }

        try {
          nc = await wsconnect({
            servers: [websocketUrl],
            authenticator: jwtAuthenticator(creds.data.token),
            reconnect: false,
          });
        } catch {
          continue;
        }
        if (cancelled) {
          void nc.close();
          return;
        }

        inputConnRef.current = { nc, subject: creds.data.subject };
        if (termRef.current !== null)
          termRef.current.options.disableStdin = false;
        setInputStatus({ kind: "on" });

        await nc.closed();
        inputConnRef.current = null;
        if (termRef.current !== null)
          termRef.current.options.disableStdin = true;
        nc = null;
      }
    }
    void run();

    // Deliberately does not reset `inputStatus`: the loop's own exits (403,
    // unavailable) set a status that must outlive the session; the button
    // handler resets it on an explicit toggle.
    return () => {
      cancelled = true;
      inputConnRef.current = null;
      if (termRef.current !== null) termRef.current.options.disableStdin = true;
      void nc?.close();
    };
  }, [jobId, inputEnabled]);

  const offersInput = view.input && canSendInput;

  return (
    <>
      {(offersInput || inputStatus.kind === "error") && (
        <p className="log-view-bar">
          {inputStatus.kind === "on" && (
            <span className="badge ok">input on</span>
          )}{" "}
          {offersInput && inputStatus.kind !== "unavailable" && (
            <button
              onClick={() => {
                setInputStatus({ kind: "off" });
                setInputEnabled((enabled) => !enabled);
              }}
            >
              {inputEnabled ? "Disable input" : "Enable console input"}
            </button>
          )}
          {inputStatus.kind === "error" && (
            <span className="error">{inputStatus.message}</span>
          )}
        </p>
      )}
      <div ref={mountRef} className="job-log-term" />
    </>
  );
}
