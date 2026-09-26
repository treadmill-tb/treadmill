import { Link, useSearchParams } from "react-router";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { LiveBadge } from "../components/badges";
import { EntityLink } from "../components/entity-link";
import { RequestError } from "../components/request-error";
import { DevBoardIcon, PlatformIcon } from "../icons";

type HostListEntry = components["schemas"]["HostListEntry"];

function matches(host: HostListEntry, q: string): boolean {
  const spec = host.spec;
  const haystack = [
    host.name,
    spec?.platform.vendor,
    ...(spec?.platform.profiles ?? []),
    ...(spec?.duts ?? []).flatMap((dut) => [dut.board, dut.vendor, dut.name]),
  ];
  return haystack.some((s) => s?.toLowerCase().includes(q) ?? false);
}

function groupBySite(hosts: HostListEntry[]): [string, HostListEntry[]][] {
  const groups = new Map<string, HostListEntry[]>();
  for (const host of hosts) {
    const site = host.spec?.site ?? "no spec";
    groups.set(site, [...(groups.get(site) ?? []), host]);
  }
  return [...groups].sort(([a], [b]) => a.localeCompare(b));
}

function HostTile({ host }: { host: HostListEntry }) {
  const spec = host.spec;
  const platform = spec?.platform;
  return (
    <article className="card host-tile">
      <p className="host-name">
        {platform != null && (
          <PlatformIcon platform={platform} size={18} aria-hidden="true" />
        )}
        <EntityLink kind="host" id={host.host_id} label={host.name} />
        <LiveBadge live={host.live} />
        {host.live && host.busy && <span className="badge warn">busy</span>}
        {host.maintenance && <span className="badge warn">maintenance</span>}
      </p>
      {spec == null ? (
        <p className="muted">no spec</p>
      ) : (
        <>
          <p className="host-facts">
            {[
              platform?.model ?? platform?.hypervisor ?? platform?.kind,
              spec.location != null
                ? `${spec.site}, ${spec.location}`
                : spec.site,
            ].join(" · ")}
          </p>
          {spec.duts.length > 0 && (
            <ul className="dut-list">
              {spec.duts.map((dut, i) => (
                <li key={i}>
                  <DevBoardIcon size={18} aria-hidden="true" />
                  <span>
                    {dut.name ?? dut.board}{" "}
                    <span className="muted">{dut.vendor}</span>
                  </span>
                </li>
              ))}
            </ul>
          )}
          <p className="host-tile-profiles">
            {spec.platform.profiles.map((p) => (
              <span key={p} className="chip mono">
                {p}
              </span>
            ))}
          </p>
        </>
      )}
    </article>
  );
}

function HostGrid({ hosts }: { hosts: HostListEntry[] }) {
  return (
    <div className="host-grid">
      {hosts.map((host) => (
        <HostTile key={host.host_id} host={host} />
      ))}
    </div>
  );
}

export default function Hosts() {
  const hosts = $api.useQuery("get", "/hosts");
  const whoami = $api.useQuery("get", "/auth/whoami");
  const [params, setParams] = useSearchParams();
  const q = params.get("q") ?? "";
  const bySite = params.get("group") === "site";

  function setParam(key: string, value: string | null) {
    setParams(
      (p) => {
        if (value === null) p.delete(key);
        else p.set(key, value);
        return p;
      },
      { replace: true },
    );
  }

  const needle = q.trim().toLowerCase();
  const shown = (hosts.data ?? []).filter(
    (host) => needle === "" || matches(host, needle),
  );

  return (
    <>
      <div className="toolbar">
        <h1>Hosts</h1>
        <span className="spacer" />
        {whoami.data?.admin === true && (
          <Link className="btn primary" to="/hosts/new">
            Register a supervisor
          </Link>
        )}
      </div>
      <div className="host-filters">
        <input
          type="search"
          aria-label="Search hosts"
          placeholder="Search name, board, vendor, profile"
          value={q}
          onChange={(e) => setParam("q", e.target.value || null)}
        />
        <label>
          <input
            type="checkbox"
            role="switch"
            checked={bySite}
            onChange={(e) =>
              setParam("group", e.target.checked ? "site" : null)
            }
          />
          Group by site
        </label>
      </div>
      {hosts.isPending && <p className="muted">Loading…</p>}
      <RequestError error={hosts.error} />
      {hosts.data &&
        (shown.length === 0 ? (
          <p className="muted">
            {hosts.data.length === 0 ? "No hosts registered." : "No matches."}
          </p>
        ) : bySite ? (
          groupBySite(shown).map(([site, group]) => (
            <section key={site}>
              <h2 className="host-group">
                {site} · {group.length}
              </h2>
              <HostGrid hosts={group} />
            </section>
          ))
        ) : (
          <HostGrid hosts={shown} />
        ))}
    </>
  );
}
