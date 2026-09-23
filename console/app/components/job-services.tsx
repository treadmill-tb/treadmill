import { ExternalLink, Globe, Plug, SquareTerminal } from "lucide-react";
import { useState } from "react";

import { client } from "../api/client";
import { ApiError, describeError, type ErrorMessages } from "../api/errors";
import type { components } from "../api/schema";
import { CopyButton } from "./copy-button";
import { Dialog } from "./dialog";

type JobServiceView = components["schemas"]["JobServiceView"];

/** The one protocol a browser can open by itself: the gateway serves it over
 * HTTPS and takes the token from the query. Every other protocol names some
 * client we cannot launch, so those services are listed but not offered. */
const BROWSER_PROTOCOL = "webapp";

/** SSH tunnelled over a websocket, which `tml` knows how to connect to. */
const SSH_PROTOCOL = "sshws";

/** The domain `tml ssh setup` routes through `tml` in the SSH config: the CLI's
 * default `ssh_domains` entry (`cli/src/config.rs`), not a gateway domain. */
const SSH_DOMAIN = "job.treadmill.dev";

type OpenState =
  | { kind: "idle" }
  | { kind: "opening"; service: string }
  /** The browser refused the tab we opened for the service; offer a link,
   * which a click of its own is allowed to follow. */
  | { kind: "blocked"; service: string; href: string }
  | { kind: "error"; message: string };

const MINT_ERRORS: ErrorMessages = {
  403: "You are not authorized to open this job's services.",
  404: "The job is no longer announcing this service.",
  409: "The job has not reported an address yet; try again shortly.",
  503: "This switchboard does not offer gateway access.",
};

/**
 * The services a job announced. A browser service opens by minting a token for
 * it and following the URL (crafted from the switchboard endpoint) with that
 * token in the query; an SSH service explains how to connect with `tml`; any
 * other protocol is only listed.
 *
 * The set arrives with the job, so a `/jobs/{id}/watch` wake-up refreshes it
 * like any other field: a service announced while the page is open shows up on
 * its own.
 */
export function JobServices({
  jobId,
  services,
  canOpen,
}: {
  jobId: string;
  services: JobServiceView[];
  canOpen: boolean;
}) {
  const [state, setState] = useState<OpenState>({ kind: "idle" });

  async function open(service: string) {
    // Opening the tab after awaiting the mint would be a popup the browser
    // blocks, so claim it while still inside the click and navigate it once
    // the token arrives.
    const tab = window.open("about:blank", "_blank");
    setState({ kind: "opening", service });

    let creds;
    try {
      creds = await client.POST("/jobs/{id}/services/{service}/token", {
        params: { path: { id: jobId, service } },
      });
    } catch {
      tab?.close();
      setState({ kind: "error", message: "Could not reach the switchboard." });
      return;
    }

    if (creds.data === undefined) {
      tab?.close();
      setState({
        kind: "error",
        message: describeError(
          new ApiError(creds.response.status, creds.error),
          MINT_ERRORS,
        ),
      });
      return;
    }

    const endpoint = creds.data.endpoints[0];
    if (!endpoint) {
      tab?.close();
      setState({
        kind: "error",
        message: "There are no endpoints for this service.",
      });
      return;
    }

    const href = `https://${endpoint.hostname}:${endpoint.port}/?tml_token=${encodeURIComponent(creds.data.token)}`;
    if (tab === null) {
      setState({ kind: "blocked", service, href });
      return;
    }
    // The token rides in the URL of a cross-origin page; sever its handle on
    // this one, which `noopener` would have done had we been able to pass it.
    tab.opener = null;
    tab.location.replace(href);
    setState({ kind: "idle" });
  }

  const [sshService, setSshService] = useState<JobServiceView | null>(null);

  return (
    <section>
      <h2>Services</h2>
      {services.length === 0 ? (
        <p className="muted">
          <em>Job does not announce any services yet</em>
        </p>
      ) : (
        <ul className="service-list">
          {services.map((service) => {
            const Icon =
              service.protocol === BROWSER_PROTOCOL
                ? Globe
                : service.protocol === SSH_PROTOCOL
                  ? SquareTerminal
                  : Plug;
            return (
              <li key={service.name}>
                <Icon size={20} aria-hidden="true" />
                <span className="service-name">
                  <strong>{service.label ?? service.name}</strong>
                  {service.label != null && (
                    <span className="mono muted">{service.name}</span>
                  )}
                </span>
                {service.protocol === BROWSER_PROTOCOL ? (
                  state.kind === "blocked" && state.service === service.name ? (
                    <a
                      href={state.href}
                      target="_blank"
                      rel="noopener noreferrer"
                    >
                      Open {service.name}
                    </a>
                  ) : (
                    <button
                      type="button"
                      disabled={!canOpen || state.kind === "opening"}
                      onClick={() => void open(service.name)}
                    >
                      {state.kind === "opening" &&
                      state.service === service.name
                        ? "Opening…"
                        : "Open"}
                      <ExternalLink size={14} aria-hidden="true" />
                    </button>
                  )
                ) : service.protocol === SSH_PROTOCOL ? (
                  <button
                    type="button"
                    disabled={!canOpen}
                    onClick={() => setSshService(service)}
                  >
                    Connect…
                  </button>
                ) : (
                  <span className="muted">
                    protocol: <span className="mono">{service.protocol}</span>
                  </span>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {state.kind === "error" && <p className="error">{state.message}</p>}
      <Dialog
        open={sshService !== null}
        onClose={() => setSshService(null)}
        title={`Connect to ${sshService?.label ?? sshService?.name ?? ""}`}
      >
        {sshService !== null && (
          <SshInstructions jobId={jobId} service={sshService.name} />
        )}
      </Dialog>
    </section>
  );
}

/** The two ways into an `sshws` service: through `tml`, or plain `ssh`. */
function SshInstructions({
  jobId,
  service,
}: {
  jobId: string;
  service: string;
}) {
  return (
    <div className="ssh-help">
      <h4>With tml</h4>
      <p>Make this the active job, then connect to it:</p>
      <Command text={`tml job set-active ${jobId}`} />
      <Command text="tml job ssh" />

      <h4>With plain ssh</h4>
      <p>
        Also works for <code>scp</code>, <code>rsync</code> and editors&apos;
        remote-SSH support. First, once per machine, let <code>tml</code> set up
        your SSH config:
      </p>
      <Command text="tml ssh setup" tag="one-time" />
      <p>Then connect with:</p>
      <Command text={`ssh ${service}-${jobId}.${SSH_DOMAIN}`} />
    </div>
  );
}

function Command({ text, tag }: { text: string; tag?: string }) {
  return (
    <div className="command">
      <code>{text}</code>
      {tag !== undefined && <span className="badge">{tag}</span>}
      <CopyButton value={text} label="Copy command" />
    </div>
  );
}
