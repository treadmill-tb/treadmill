import { ChevronDown, ChevronUp, Cpu } from "lucide-react";
import { Fragment, useState, type ReactNode } from "react";

import { hostStatus, STATUS_TONE, type HostStatus } from "../api/hosts";
import type { components } from "../api/schema";
import type { Tone } from "./badges";
import { RelTime } from "./rel-time";

type HostListEntry = components["schemas"]["HostListEntry"];
type HostMatch = components["schemas"]["HostMatch"];
type Report = components["schemas"]["HostRequirementsReport"];

export type HostCandidate = {
  match: HostMatch;
  host: HostListEntry | undefined;
};

export type Verdict = {
  tone: Tone | "idle";
  title: string;
  facts: ReactNode[];
};

const STATUS_ORDER: HostStatus[] = ["free", "busy", "maintenance", "offline"];

export function byStatus(a: HostCandidate, b: HostCandidate): number {
  return (
    STATUS_ORDER.indexOf(hostStatus(a.host)) -
    STATUS_ORDER.indexOf(hostStatus(b.host))
  );
}

export function failureVerdict(report: Report): Verdict | null {
  const eligible = report.hosts.filter((h) => h.schedulable).length;
  if (report.compile_error != null) {
    return {
      tone: "danger",
      title: "Syntax error",
      facts: [<code key="error">{report.compile_error}</code>],
    };
  }
  if (report.authorized === 0) {
    return {
      tone: "danger",
      title: "No usable hosts",
      facts: ["No host you may start jobs on"],
    };
  }
  if (eligible > 0) return null;
  if (report.predicate_matched === 0 && report.errored > 0) {
    return {
      tone: "danger",
      title: "Filter error",
      facts: [
        `Failed on ${report.errored} / ${report.authorized} hosts`,
        <code key="error">{report.errors[0]?.message}</code>,
      ],
    };
  }
  if (report.predicate_matched === 0) {
    return {
      tone: "danger",
      title: "No matching hosts",
      facts: [`0 / ${report.authorized} hosts match`],
    };
  }
  return {
    tone: "danger",
    title: "No compatible hosts",
    facts: [
      `${report.predicate_matched} matching, not compatible with this image`,
    ],
  };
}

export function availabilityVerdict(eligible: HostCandidate[]): Verdict {
  const total = eligible.length;
  const free = eligible.filter((c) => hostStatus(c.host) === "free");
  if (free.length > 0) {
    return {
      tone: "ok",
      title: "Hosts available",
      facts: [`${free.length} / ${total} eligible free`],
    };
  }
  const busy = eligible.filter((c) => hostStatus(c.host) === "busy");
  if (busy.length > 0) {
    const leaseEnds = busy
      .map((c) => c.host?.current_lease_expires_at)
      .filter((t): t is string => t != null)
      .sort()[0];
    return {
      tone: "warn",
      title: "Hosts busy",
      facts: [
        `${busy.length} / ${total} eligible busy`,
        ...(leaseEnds === undefined
          ? []
          : [
              <span key="ends">
                Current lease ends <RelTime iso={leaseEnds} />
              </span>,
            ]),
      ],
    };
  }
  return {
    tone: "warn",
    title: "Hosts unavailable",
    facts: eligible
      .slice(0, 3)
      .map((c) => `${c.match.name}: ${hostStatus(c.host)}`),
  };
}

function StatusText({ host }: { host: HostListEntry | undefined }) {
  const status = hostStatus(host);
  if (status === "busy" && host?.current_lease_expires_at != null) {
    return (
      <>
        busy · lease ends <RelTime iso={host.current_lease_expires_at} />
      </>
    );
  }
  return <>{status}</>;
}

type Dut = NonNullable<HostListEntry["spec"]>["duts"][number];

function dutSummary(duts: Dut[]): string {
  const counts = new Map<string, number>();
  for (const dut of duts)
    counts.set(dut.board, (counts.get(dut.board) ?? 0) + 1);
  return [...counts]
    .map(([board, n]) => (n > 1 ? `${n}× ${board}` : board))
    .join(", ");
}

function formatMemory(mb: number): string {
  const gb = mb / 1024;
  return `${gb < 10 ? gb.toFixed(1).replace(/\.0$/, "") : Math.round(gb)} GB`;
}

export function HostLine({
  candidate,
  action,
}: {
  candidate: HostCandidate;
  action?: ReactNode;
}) {
  const { match, host } = candidate;
  const incompatible = match.predicate_matched && !match.schedulable;
  const spec = host?.spec;
  const duts = spec?.duts ?? [];
  return (
    <span className={`host-item${incompatible ? " dim" : ""}`}>
      <span className={`dot ${STATUS_TONE[hostStatus(host)]}`} />
      <strong>{match.name}</strong>
      <span className="host-item-actions">{action}</span>
      <small className="host-item-meta">
        {incompatible ? (
          <span className="incompatible">not compatible with this image</span>
        ) : (
          <StatusText host={host} />
        )}
        {match.platform_profile != null && (
          <>
            {" · "}
            <span className="mono">{match.platform_profile}</span>
          </>
        )}
        {spec != null && <> · {spec.site}</>}
      </small>
      {duts.length > 0 && (
        <small className="host-item-meta">
          <Cpu size={12} aria-hidden="true" /> {dutSummary(duts)}
        </small>
      )}
    </span>
  );
}

function HostDetails({ spec }: { spec: NonNullable<HostListEntry["spec"]> }) {
  const tags = [
    ...spec.platform.profiles,
    ...Object.entries(spec.labels).map(([key, value]) => `${key}=${value}`),
  ];
  return (
    <div className="host-details">
      <span>
        {[
          spec.location != null ? `${spec.site}, ${spec.location}` : spec.site,
          `${spec.platform.kind} ${spec.platform.arch}`,
          `${spec.resources.cpu_cores} cores`,
          formatMemory(spec.resources.memory_mb),
          `${spec.resources.storage_gb} GB storage`,
        ].join(" · ")}
      </span>
      {spec.duts.length > 0 && (
        <ul>
          {spec.duts.map((dut, i) => (
            <li key={i}>
              <Cpu size={12} aria-hidden="true" />{" "}
              {dut.name != null && <>{dut.name} </>}
              <span className={dut.name != null ? "muted" : undefined}>
                {dut.vendor} {dut.board}
              </span>
            </li>
          ))}
        </ul>
      )}
      {tags.length > 0 && (
        <span>
          {tags.map((tag) => (
            <span key={tag} className="chip mono">
              {tag}
            </span>
          ))}
        </span>
      )}
      {spec.description != null && (
        <span className="muted">{spec.description}</span>
      )}
    </div>
  );
}

export function HostItem({
  candidate,
  action,
}: {
  candidate: HostCandidate;
  action?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const spec = candidate.host?.spec;
  return (
    <div className="host-item-card">
      <HostLine
        candidate={candidate}
        action={
          <>
            {action}
            {spec != null && (
              <button
                type="button"
                className="icon-btn"
                aria-label="Host details"
                aria-expanded={open}
                onClick={() => setOpen(!open)}
              >
                {open ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
              </button>
            )}
          </>
        }
      />
      {open && spec != null && <HostDetails spec={spec} />}
    </div>
  );
}

export function VerdictBox({ verdict }: { verdict: Verdict }) {
  return (
    <div className={`verdict ${verdict.tone}`}>
      <strong>{verdict.title}</strong>
      {verdict.facts.map((fact, i) => (
        <span key={i}>{fact}</span>
      ))}
    </div>
  );
}

function Funnel({ report }: { report: Report }) {
  const rows: [string, number][] = [
    ["Usable", report.authorized],
    ["Match filter", report.predicate_matched],
  ];
  if (report.image_matched != null) {
    rows.push([
      "Compatible with image",
      report.hosts.filter((h) => h.schedulable).length,
    ]);
  }
  return (
    <section>
      <h3>Hosts</h3>
      <div className="funnel">
        {rows.map(([label, count]) => (
          <div key={label} className="funnel-row">
            <span className="funnel-bar">
              <span
                className={count === 0 ? "empty" : undefined}
                style={{
                  width: `${report.authorized === 0 ? 0 : (count / report.authorized) * 100}%`,
                }}
              />
              <span>{label}</span>
            </span>
            <strong>{count}</strong>
          </div>
        ))}
      </div>
    </section>
  );
}

export function splitCandidates(
  report: Report | undefined,
  hosts: HostListEntry[] | undefined,
): { eligible: HostCandidate[]; incompatible: HostCandidate[] } {
  const candidate = (match: HostMatch): HostCandidate => ({
    match,
    host: hosts?.find((h) => h.host_id === match.host_id),
  });
  const matches = report?.hosts ?? [];
  return {
    eligible: matches
      .filter((h) => h.schedulable)
      .map(candidate)
      .sort(byStatus),
    incompatible: matches
      .filter((h) => h.predicate_matched && !h.schedulable)
      .map(candidate),
  };
}

export function matchVerdict(
  report: Report,
  eligible: HostCandidate[],
): Verdict {
  return failureVerdict(report) ?? availabilityVerdict(eligible);
}

type SummaryProps = {
  verdict: Verdict;
  hostsTitle: string;
  hosts: HostCandidate[];
  onSelectHost?: (hostId: string) => void;
  report: Report | undefined;
  showFunnel: boolean;
};

export function HostMatchSummary({
  verdict,
  hostsTitle,
  hosts,
  onSelectHost,
  report,
  showFunnel,
}: SummaryProps) {
  const errored =
    report !== undefined && report.errored > 0 && report.predicate_matched > 0;
  return (
    <div className="match-summary">
      <VerdictBox verdict={verdict} />

      {hosts.length > 0 && (
        <section>
          <h3>{hostsTitle}</h3>
          <div className="host-items">
            {hosts.map((c) => (
              <HostItem
                key={c.match.host_id}
                candidate={c}
                action={
                  onSelectHost !== undefined &&
                  c.match.schedulable && (
                    <button
                      type="button"
                      className="link-btn"
                      onClick={() => onSelectHost(c.match.host_id)}
                    >
                      Select
                    </button>
                  )
                }
              />
            ))}
          </div>
        </section>
      )}

      {showFunnel && report !== undefined && report.compile_error == null && (
        <Funnel report={report} />
      )}

      {errored && (
        <section>
          <h3>Filter errors · {report.errored}</h3>
          <ul className="filter-errors">
            {report.errors.slice(0, 3).map((e) => (
              <li key={e.host_id}>
                {e.name}: <code>{e.message}</code>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

export function JobPreview({
  facts,
  ...summary
}: SummaryProps & { facts: [string, ReactNode][] }) {
  return (
    <aside className="job-preview" aria-live="polite">
      <HostMatchSummary {...summary} />
      <section>
        <h3>Job Preview</h3>
        <dl className="props">
          {facts.map(([label, value]) => (
            <Fragment key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </Fragment>
          ))}
        </dl>
      </section>
    </aside>
  );
}
