import { useState } from "react";

import { client } from "../api/client";
import type { components } from "../api/schema";

type JobServiceView = components["schemas"]["JobServiceView"];

/** The one protocol a browser can open by itself: the gateway serves it over
 * HTTPS and takes the token from the query. Every other protocol names some
 * client we cannot launch, so those services are listed but not offered. */
const BROWSER_PROTOCOL = "webapp";

type OpenState =
  | { kind: "idle" }
  | { kind: "opening"; service: string }
  /** The browser refused the tab we opened for the service; offer a link,
   * which a click of its own is allowed to follow. */
  | { kind: "blocked"; service: string; href: string }
  | { kind: "error"; message: string };

function mintFailure(status: number): string {
  switch (status) {
    case 403:
      return "You are not authorized to open this job's services.";
    case 404:
      return "The job is no longer announcing this service.";
    case 409:
      return "The job has not reported an address yet; try again shortly.";
    case 503:
      return "This switchboard does not offer gateway access.";
    default:
      return `Could not mint a token for this service (HTTP ${status}).`;
  }
}

/**
 * The services a job announced, each opened by minting a token for it and
 * following the URL the switchboard returns with that token in the query.
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
      setState({ kind: "error", message: mintFailure(creds.response.status) });
      return;
    }

    const href = `${creds.data.url}?tml_token=${encodeURIComponent(creds.data.token)}`;
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

  return (
    <section>
      <h2>Services</h2>
      {services.length === 0 ? (
        <p className="muted">No services announced.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Label</th>
              <th>Protocol</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {services.map((service) => (
              <tr key={service.name}>
                <td className="mono">{service.name}</td>
                <td>{service.label ?? <span className="muted">—</span>}</td>
                <td>
                  <span className="badge">{service.protocol}</span>
                </td>
                <td>
                  {service.protocol !== BROWSER_PROTOCOL ? (
                    <span className="muted">—</span>
                  ) : state.kind === "blocked" &&
                    state.service === service.name ? (
                    <a
                      href={state.href}
                      target="_blank"
                      rel="noopener noreferrer"
                    >
                      Open {service.name}
                    </a>
                  ) : (
                    <button
                      disabled={!canOpen || state.kind === "opening"}
                      onClick={() => void open(service.name)}
                    >
                      {state.kind === "opening" &&
                      state.service === service.name
                        ? "Opening…"
                        : "Open"}
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {state.kind === "error" && <p className="error">{state.message}</p>}
    </section>
  );
}
