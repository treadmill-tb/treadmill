import { Cpu, Server } from "lucide-react";
import { useState } from "react";

import type { components } from "../api/schema";
import { LiveBadge } from "./badges";
import { EntityLink } from "./entity-link";

type HostListEntry = components["schemas"]["HostListEntry"];

/** Devices listed before "Show all". */
const DUTS_SHOWN = 2;

function formatMemory(mb: number): string {
  const gb = mb / 1024;
  return `${gb < 10 ? gb.toFixed(1).replace(/\.0$/, "") : Math.round(gb)} GB`;
}

/** The host a job runs on, led by the devices attached to it. */
export function HostCard({ host }: { host: HostListEntry }) {
  const [more, setMore] = useState(false);
  const [allDuts, setAllDuts] = useState(false);
  const spec = host.spec;
  const duts = spec?.duts ?? [];
  const shown = allDuts ? duts : duts.slice(0, DUTS_SHOWN);
  const labels = Object.entries(spec?.labels ?? {});

  return (
    <aside className="card host-card">
      <h3 className="card-head">
        <Server size={18} aria-hidden="true" />
        Assigned Host
      </h3>
      <p className="host-name">
        <EntityLink kind="host" id={host.host_id} label={host.name} />
        <LiveBadge live={host.live} />
        {host.maintenance && <span className="badge warn">maintenance</span>}
      </p>
      {spec != null && (
        <p className="host-facts">
          {[
            spec.location != null
              ? `${spec.site}, ${spec.location}`
              : spec.site,
            `${spec.platform.kind} ${spec.platform.arch}`,
            `${spec.resources.cpu_cores} cores`,
            `${formatMemory(spec.resources.memory_mb)} memory`,
          ].join(" · ")}
        </p>
      )}

      <h4>
        Devices
        {duts.length > 0 && <span className="muted"> ({duts.length})</span>}
      </h4>
      {spec == null ? (
        <p className="muted">This host has not been described yet.</p>
      ) : duts.length === 0 ? (
        <p className="muted">No devices attached.</p>
      ) : (
        <ul className="dut-list">
          {shown.map((dut, i) => (
            <li key={i}>
              <Cpu size={18} aria-hidden="true" />
              <span>
                {dut.name != null && <>{dut.name} </>}
                <span className={dut.name != null ? "muted" : undefined}>
                  {dut.vendor} {dut.board}
                </span>
              </span>
            </li>
          ))}
        </ul>
      )}
      {duts.length > DUTS_SHOWN && (
        <button
          type="button"
          className="link-btn"
          onClick={() => setAllDuts((a) => !a)}
        >
          {allDuts ? "Show fewer devices" : `Show all ${duts.length} devices`}
        </button>
      )}

      {more && spec != null && (
        <dl className="props more-props">
          {spec.description != null && (
            <>
              <dt>Description</dt>
              <dd>{spec.description}</dd>
            </>
          )}
          <dt>Storage</dt>
          <dd>{spec.resources.storage_gb} GB</dd>
          {spec.platform.profiles.length > 0 && (
            <>
              <dt>Profiles</dt>
              <dd>
                {spec.platform.profiles.map((p) => (
                  <span key={p} className="badge mono">
                    {p}
                  </span>
                ))}
              </dd>
            </>
          )}
          {labels.length > 0 && (
            <>
              <dt>Labels</dt>
              <dd>
                {labels.map(([key, value]) => (
                  <span key={key} className="badge mono">
                    {key}={value}
                  </span>
                ))}
              </dd>
            </>
          )}
        </dl>
      )}
      {spec != null && (
        <p className="details-more">
          <button
            type="button"
            className="link-btn"
            aria-expanded={more}
            onClick={() => setMore((m) => !m)}
          >
            {more ? "Fewer details" : "More details"}
          </button>
        </p>
      )}
    </aside>
  );
}
