import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import { Minus, Plus } from "lucide-react";
import { useState, type FormEvent, type ReactNode } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";

import { $api } from "../api/client";
import {
  jobImageSpec,
  parseSingleHostPredicate,
  singleHostPredicate,
} from "../api/hosts";
import { isStandard, shortDigest } from "../api/images";
import type { components } from "../api/schema";
import { ShortId } from "../components/entity-link";
import { HostChoice, type HostMode } from "../components/host-choice";
import { ImageChoice } from "../components/image-choice";
import { JobInfoName } from "../components/job-name";
import {
  availabilityVerdict,
  HostItem,
  JobPreview,
  matchVerdict,
  splitCandidates,
  type HostCandidate,
  type Verdict,
} from "../components/job-preview";
import { RequestError } from "../components/request-error";
import { useDebounced } from "../hooks/use-debounced";

type ImageSetInfo = components["schemas"]["ImageSetInfo"];
type JobImageReference = components["schemas"]["JobImageReference"];
type JobInfo = components["schemas"]["JobInfo"];
type JobInitSpec = components["schemas"]["JobInitSpec"];
type JobLeaseExpiryAction = components["schemas"]["JobLeaseExpiryAction"];
type JobParameter = components["schemas"]["JobParameter"];

type ParamRow = {
  name: string;
  value: string;
  secret: boolean;
  reenter: boolean;
};
type Base = { job: JobInfo; mode: "resume" | "restart" };

const LAST_IMAGE_KEY = "tml_last_image";

function lastImage(): string | null {
  try {
    return localStorage.getItem(LAST_IMAGE_KEY);
  } catch {
    return null;
  }
}

function rememberImage(id: string) {
  try {
    localStorage.setItem(LAST_IMAGE_KEY, id);
  } catch {
    // Preselection only.
  }
}

const LEASE_PRESETS = [1800, 3600, 7200, 8 * 3600, 86400];

function formatDuration(secs: number): string {
  const days = secs / 86400;
  if (Number.isInteger(days)) return days === 1 ? "1 day" : `${days} days`;
  const hours = Math.floor(secs / 3600);
  const minutes = Math.floor((secs % 3600) / 60);
  if (hours === 0) return `${minutes} min`;
  return minutes === 0 ? `${hours} h` : `${hours} h ${minutes} min`;
}

function defaultImage(
  sets: ImageSetInfo[] | undefined,
  requested: string | null,
): string | null {
  const runnable = (sets ?? []).filter((s) => s.latest_generation != null);
  const preferred = [requested, lastImage()].find((id) =>
    runnable.some((s) => s.id === id),
  );
  return preferred ?? runnable.find(isStandard)?.id ?? null;
}

function describeImage(
  reference: JobImageReference,
  sets: ImageSetInfo[] | undefined,
): ReactNode {
  if (reference.type === "image") {
    return (
      <span className="mono">{shortDigest(reference.manifest_digest)}</span>
    );
  }
  const set = sets?.find((s) => s.id === reference.set_id);
  return `${set?.display_name ?? "Image"} v${reference.generation}`;
}

function initialHost(base: Base | null, requestedHost: string | null) {
  const predicate = base?.job.host_cel_predicate ?? "true";
  const single = requestedHost ?? parseSingleHostPredicate(predicate);
  if (single !== null) {
    return { mode: "single" as const, hostId: single, filter: "" };
  }
  if (predicate === "true") {
    return { mode: "any" as const, hostId: null, filter: "" };
  }
  return { mode: "filter" as const, hostId: null, filter: predicate };
}

export default function JobNew() {
  const [searchParams] = useSearchParams();
  const resume = searchParams.get("resume");
  const restart = searchParams.get("restart");
  const baseId = resume ?? restart;
  const base = $api.useQuery(
    "get",
    "/jobs/{id}",
    { params: { path: { id: baseId ?? "" } } },
    { enabled: baseId !== null },
  );
  const me = $api.useQuery("get", "/users/me");

  if (baseId === null) return <JobForm base={null} />;
  if (base.isError) {
    return (
      <RequestError
        error={base.error}
        messages={{ 403: "No such job, or you don't have access to it." }}
      />
    );
  }
  if (base.data === undefined || me.data === undefined) {
    return <p className="muted">Loading…</p>;
  }
  return (
    <JobForm
      key={baseId}
      base={{ job: base.data, mode: resume !== null ? "resume" : "restart" }}
    />
  );
}

function LeaseDuration({
  defaultSecs,
  secs,
  onSecs,
  custom,
  onCustom,
  expiry,
  onExpiry,
}: {
  defaultSecs: number | undefined;
  secs: number | null;
  onSecs: (secs: number | null) => void;
  custom: string | null;
  onCustom: (custom: string | null) => void;
  expiry: JobLeaseExpiryAction;
  onExpiry: (expiry: JobLeaseExpiryAction) => void;
}) {
  const options = [
    ...new Set([
      ...LEASE_PRESETS,
      ...(defaultSecs === undefined ? [] : [defaultSecs]),
      ...(secs === null ? [] : [secs]),
    ]),
  ].sort((a, b) => a - b);
  const selected = custom === null ? (secs ?? defaultSecs) : undefined;

  return (
    <fieldset className="form-section">
      <legend>Lease duration</legend>
      <div className="toggles">
        {options.map((option) => (
          <button
            key={option}
            type="button"
            aria-pressed={selected === option}
            onClick={() => {
              onCustom(null);
              onSecs(option === defaultSecs ? null : option);
            }}
          >
            {formatDuration(option)}
            {option === defaultSecs && " (default)"}
          </button>
        ))}
        <button
          type="button"
          aria-pressed={custom !== null}
          onClick={() => onCustom(custom ?? "")}
        >
          Custom
        </button>
      </div>
      {custom !== null && (
        <input
          aria-label="Custom lease duration"
          placeholder="e.g. 90m, 3h, 2d"
          autoFocus
          value={custom}
          onChange={(e) => onCustom(e.target.value)}
        />
      )}
      <div className="inline-options">
        <span className="muted">On expiry</span>
        <label>
          <input
            type="radio"
            name="lease-expiry"
            checked={expiry === "terminate"}
            onChange={() => onExpiry("terminate")}
          />
          Terminate
        </label>
        <label>
          <input
            type="radio"
            name="lease-expiry"
            checked={expiry === "preempt"}
            onChange={() => onExpiry("preempt")}
          />
          Keep running, preemptible
        </label>
      </div>
    </fieldset>
  );
}

function Parameters({
  rows,
  onRows,
}: {
  rows: ParamRow[];
  onRows: (rows: ParamRow[]) => void;
}) {
  const [open] = useState(rows.some((r) => r.reenter));
  const missing = rows.filter((r) => r.reenter && r.value === "").length;
  const secrets = rows.filter((r) => r.secret).length;
  const update = (i: number, change: Partial<ParamRow>) =>
    onRows(rows.map((r, j) => (j === i ? { ...r, ...change } : r)));

  return (
    <details className="form-details" open={open}>
      <summary>
        <span>Parameters</span>
        <small className={missing > 0 ? "incompatible" : "muted"}>
          {missing > 0
            ? `${missing} secret missing`
            : rows.length === 0
              ? "none"
              : `${rows.length}${secrets > 0 ? ` · ${secrets} secret` : ""}`}
        </small>
      </summary>
      {rows.map((row, i) => (
        <div className="field-row" key={i}>
          <input
            placeholder="name"
            aria-label="Parameter name"
            className="mono"
            value={row.name}
            onChange={(e) => update(i, { name: e.target.value })}
          />
          <input
            type={row.secret ? "password" : "text"}
            placeholder={row.reenter ? "re-enter secret" : "value"}
            aria-label="Parameter value"
            aria-invalid={row.reenter && row.value === "" ? true : undefined}
            className="mono"
            value={row.value}
            onChange={(e) => update(i, { value: e.target.value })}
          />
          <label className="check">
            <input
              type="checkbox"
              checked={row.secret}
              onChange={(e) => update(i, { secret: e.target.checked })}
            />
            secret
          </label>
          <button
            type="button"
            onClick={() => onRows(rows.filter((_, j) => j !== i))}
          >
            Remove
          </button>
        </div>
      ))}
      <div>
        <button
          type="button"
          onClick={() =>
            onRows([
              ...rows,
              { name: "", value: "", secret: false, reenter: false },
            ])
          }
        >
          Add parameter
        </button>
      </div>
    </details>
  );
}

function JobForm({ base }: { base: Base | null }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [searchParams] = useSearchParams();
  const job = base?.job;
  const resuming = base?.mode === "resume";

  const me = $api.useQuery("get", "/users/me");
  const sets = $api.useQuery("get", "/image-sets");
  const hosts = $api.useQuery("get", "/hosts");
  const defaults = $api.useQuery("get", "/jobs/defaults");
  const enqueue = $api.useMutation("post", "/jobs", {
    onSuccess: async (data) => {
      await queryClient.invalidateQueries({ queryKey: ["jobs"] });
      await navigate(`/jobs/${data.job_id}`);
    },
  });

  const [initial] = useState(() => initialHost(base, searchParams.get("host")));
  const [label, setLabel] = useState(job?.label ?? "");
  const [picked, setPicked] = useState<string | null>(null);
  const [version, setVersion] = useState<number | null>(null);
  const [hostMode, setHostMode] = useState<HostMode>(initial.mode);
  const [hostId, setHostId] = useState<string | null>(initial.hostId);
  const [filter, setFilter] = useState(initial.filter);
  const [leaseSecs, setLeaseSecs] = useState<number | null>(
    job?.lease_duration_secs ?? null,
  );
  const [customLease, setCustomLease] = useState<string | null>(null);
  const [expiry, setExpiry] = useState<JobLeaseExpiryAction>(
    job?.lease_expiry_action ?? "terminate",
  );
  const [paramRows, setParamRows] = useState<ParamRow[]>(() =>
    Object.entries(job?.parameters ?? {}).map(([name, p]) => ({
      name,
      value: p.value ?? "",
      secret: p.secret,
      reenter: p.secret,
    })),
  );
  const [owner, setOwner] = useState(
    job?.owner_id == null || job.owner_id === me.data?.user_id
      ? ""
      : job.owner_id,
  );
  const [restarts, setRestarts] = useState(0);

  const setId =
    base === null
      ? (picked ?? defaultImage(sets.data, searchParams.get("image")))
      : null;
  const selectedSet = sets.data?.find((s) => s.id === setId);
  const ownerId = owner === "" ? null : owner;

  let initSpec: JobInitSpec | null = null;
  if (base === null) {
    if (setId !== null) {
      initSpec = { type: "image_set", set_id: setId, generation: version };
    }
  } else {
    initSpec = jobImageSpec(base.job);
  }

  let predicate = "true";
  if (job !== undefined && resuming) predicate = job.host_cel_predicate;
  else if (hostMode === "single" && hostId !== null)
    predicate = singleHostPredicate(hostId);
  else if (hostMode === "filter" && filter.trim() !== "")
    predicate = filter.trim();

  const matchPredicate = useDebounced(predicate, 300);
  const match = $api.useQuery(
    "post",
    "/hosts/match",
    {
      body: {
        host_cel_predicate: matchPredicate,
        init_spec: initSpec,
        owner: ownerId,
      },
    },
    { enabled: !resuming, placeholderData: keepPreviousData },
  );
  const everyHost = $api.useQuery(
    "post",
    "/hosts/match",
    {
      body: { host_cel_predicate: "true", init_spec: initSpec, owner: ownerId },
    },
    { enabled: !resuming, placeholderData: keepPreviousData },
  );

  const hostById = new Map((hosts.data ?? []).map((h) => [h.host_id, h]));
  const report = match.data;
  const { eligible, incompatible } = splitCandidates(report, hosts.data);
  const every = splitCandidates(everyHost.data, hosts.data);
  const candidates = [...every.eligible, ...every.incompatible];

  const resumeHostId = job?.dispatched_on_host_id ?? "";
  const resumeHost: HostCandidate = {
    match: {
      host_id: resumeHostId,
      name: hostById.get(resumeHostId)?.name ?? resumeHostId,
      predicate_matched: true,
      schedulable: true,
    },
    host: hostById.get(resumeHostId),
  };

  let verdict: Verdict;
  if (resuming) verdict = availabilityVerdict([resumeHost]);
  else if (initSpec === null)
    verdict = { tone: "idle", title: "No image selected", facts: [] };
  else if (hostMode === "single" && hostId === null)
    verdict = { tone: "idle", title: "No host selected", facts: [] };
  else if (report === undefined)
    verdict = { tone: "idle", title: "Checking hosts…", facts: [] };
  else verdict = matchVerdict(report, eligible);

  let previewHosts = [...eligible, ...incompatible];
  if (resuming) previewHosts = [resumeHost];
  else if (hostMode === "single" && hostId === null) previewHosts = [];
  const single = resuming || hostMode === "single";
  const selectedHost = candidates.find((c) => c.match.host_id === hostId);

  let hostFact: ReactNode;
  if (resuming) hostFact = resumeHost.match.name;
  else if (hostMode === "single")
    hostFact = selectedHost?.match.name ?? <span className="muted">—</span>;
  else
    hostFact = (
      <>
        {hostMode === "any" ? "any" : "filter"}
        {report !== undefined && (
          <span className="muted"> · {eligible.length} eligible</span>
        )}
      </>
    );

  const defaultSecs = defaults.data?.lease_duration_secs;
  const effectiveLease = leaseSecs ?? defaultSecs;
  const leaseFact =
    customLease !== null
      ? customLease.trim() || "—"
      : effectiveLease === undefined
        ? "default"
        : formatDuration(effectiveLease);

  const ownerName =
    owner === ""
      ? "you"
      : (me.data?.groups.find((g) => g.group_id === owner)?.name ?? owner);
  const secretCount = paramRows.filter((r) => r.secret).length;
  const facts: [string, ReactNode][] = [
    ["Name", label.trim() || <span className="muted">—</span>],
    [
      "Image",
      job !== undefined ? (
        describeImage(job.image.reference, sets.data)
      ) : selectedSet === undefined ? (
        <span className="muted">—</span>
      ) : (
        <>
          {selectedSet.display_name}{" "}
          <span className="muted">
            {version === null
              ? `latest (v${selectedSet.latest_generation})`
              : `v${version}`}
          </span>
        </>
      ),
    ],
    ["Host", hostFact],
    [
      "Lease",
      <>
        {leaseFact}{" "}
        <span className="muted">
          · then {expiry === "terminate" ? "terminate" : "preemptible"}
        </span>
      </>,
    ],
    ["Owner", ownerName],
    [
      "Parameters",
      paramRows.length === 0 ? (
        <span className="muted">none</span>
      ) : (
        <>
          {paramRows.length}
          {secretCount > 0 && (
            <span className="muted"> · {secretCount} secret</span>
          )}
        </>
      ),
    ],
  ];
  if (!resuming) facts.push(["Auto restarts", String(restarts)]);

  function changeHostMode(mode: HostMode) {
    if (mode === "filter" && hostMode === "single" && hostId !== null) {
      setFilter(singleHostPredicate(hostId));
    }
    setHostMode(mode);
  }

  function selectHost(id: string) {
    setHostId(id);
    setHostMode("single");
  }

  const missingSecret = paramRows.some((r) => r.reenter && r.value === "");
  const action = base === null ? "Enqueue" : resuming ? "Resume" : "Restart";

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    let init_spec: JobInitSpec;
    if (base !== null) {
      init_spec = { type: base.mode, job_id: base.job.job_id };
    } else {
      if (setId === null) return;
      rememberImage(setId);
      init_spec = { type: "image_set", set_id: setId, generation: version };
    }
    const parameters: Record<string, JobParameter> = {};
    for (const row of paramRows) {
      if (row.name !== "") {
        parameters[row.name] = { value: row.value, secret: row.secret };
      }
    }
    let lease_duration: string | null = null;
    if (customLease !== null) lease_duration = customLease.trim() || null;
    else if (leaseSecs !== null && leaseSecs !== defaultSecs)
      lease_duration = String(leaseSecs);

    enqueue.mutate({
      body: {
        init_spec,
        label: label.trim() === "" ? null : label.trim(),
        host_cel_predicate: predicate,
        parameters,
        restart_policy: { max_restarts: resuming ? 0 : restarts },
        owner: ownerId,
        lease_duration,
        lease_expiry_action: expiry,
      },
    });
  }

  return (
    <>
      <h1>
        {job === undefined ? (
          "Enqueue Job"
        ) : (
          <>
            {action} Job <JobInfoName job={job} />
          </>
        )}
      </h1>
      <form className="enqueue" onSubmit={onSubmit}>
        <div className="enqueue-form">
          <label className="field">
            <span>Name</span>
            <input
              maxLength={256}
              placeholder="optional"
              value={label}
              onChange={(e) => setLabel(e.target.value)}
            />
          </label>

          {job !== undefined ? (
            <fieldset className="options">
              <legend>Image</legend>
              <div className="fixed">
                <span className="option-title">
                  <strong>
                    {describeImage(job.image.reference, sets.data)}
                  </strong>
                  <span className="muted">
                    from <JobInfoName job={job} /> (
                    <Link to={`/jobs/${job.job_id}`}>
                      <ShortId id={job.job_id} />
                    </Link>
                    )
                  </span>
                </span>
              </div>
            </fieldset>
          ) : sets.data === undefined ? (
            <>
              {sets.isPending && <p className="muted">Loading images…</p>}
              <RequestError error={sets.error} />
            </>
          ) : (
            <ImageChoice
              sets={sets.data}
              value={setId}
              onChange={setPicked}
              version={version}
              onVersion={setVersion}
              host={
                hostMode === "single" && hostId !== null
                  ? hostById.get(hostId)
                  : undefined
              }
            />
          )}

          {resuming ? (
            <fieldset className="options">
              <legend>Host</legend>
              <div className="fixed">
                <HostItem candidate={resumeHost} />
              </div>
            </fieldset>
          ) : (
            <HostChoice
              mode={hostMode}
              onMode={changeHostMode}
              hostId={hostId}
              onHostId={setHostId}
              filter={filter}
              onFilter={setFilter}
              candidates={candidates}
            />
          )}

          <LeaseDuration
            defaultSecs={defaultSecs}
            secs={leaseSecs}
            onSecs={setLeaseSecs}
            custom={customLease}
            onCustom={setCustomLease}
            expiry={expiry}
            onExpiry={setExpiry}
          />

          <Parameters rows={paramRows} onRows={setParamRows} />

          <details className="form-details">
            <summary>
              <span>More options</span>
              <small className="muted">
                owner: {ownerName}
                {!resuming && ` · restarts: ${restarts}`}
              </small>
            </summary>
            <label className="inline-field">
              Owner
              <select value={owner} onChange={(e) => setOwner(e.target.value)}>
                <option value="">
                  {me.data ? `${me.data.name} (you)` : "you"}
                </option>
                {me.data?.groups.map((g) => (
                  <option key={g.group_id} value={g.group_id}>
                    {g.name}
                  </option>
                ))}
              </select>
            </label>
            {!resuming && (
              <div className="inline-field">
                Auto restarts
                <span className="stepper">
                  <button
                    type="button"
                    aria-label="Fewer restarts"
                    disabled={restarts === 0}
                    onClick={() => setRestarts(restarts - 1)}
                  >
                    <Minus size={14} aria-hidden="true" />
                  </button>
                  <output aria-live="polite">{restarts}</output>
                  <button
                    type="button"
                    aria-label="More restarts"
                    onClick={() => setRestarts(restarts + 1)}
                  >
                    <Plus size={14} aria-hidden="true" />
                  </button>
                </span>
              </div>
            )}
          </details>
        </div>

        <JobPreview
          verdict={verdict}
          hostsTitle={single ? "Host" : `Eligible hosts · ${eligible.length}`}
          hosts={previewHosts}
          onSelectHost={single ? undefined : selectHost}
          report={resuming ? undefined : report}
          showFunnel={!resuming && hostMode === "filter"}
          facts={facts}
        />

        <div className="enqueue-submit">
          <RequestError
            error={match.error}
            messages={{
              403: "You can't use this image, or can't run jobs as this owner.",
            }}
          />
          <RequestError
            error={enqueue.error}
            messages={{
              403: "You are not allowed to start this job.",
              422: "This job can't be resumed.",
              404: "No such image, or it isn't shared with you.",
            }}
          />
          <div className="toolbar">
            <small className="muted">
              {initSpec === null
                ? "no image"
                : missingSecret
                  ? "secret parameter missing"
                  : verdict.tone === "danger"
                    ? "no eligible host"
                    : null}
            </small>
            <button
              type="submit"
              className={verdict.tone === "danger" ? "secondary" : undefined}
              disabled={enqueue.isPending || initSpec === null || missingSecret}
            >
              {enqueue.isPending
                ? "Enqueuing…"
                : verdict.tone === "danger"
                  ? `${action} anyway`
                  : action}
            </button>
          </div>
        </div>
      </form>
    </>
  );
}
