import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";

import { $api } from "../api/client";
import { isStandard } from "../api/images";
import type { components } from "../api/schema";
import { isUuid } from "../api/subjects";
import { ShortId } from "../components/entity-link";
import { HelpTip } from "../components/help-tip";
import { ImageRef } from "../components/image-ref";
import { RequestError } from "../components/request-error";

type JobInfo = components["schemas"]["JobInfo"];
type JobInitSpec = components["schemas"]["JobInitSpec"];
type JobParameter = components["schemas"]["JobParameter"];

type ParamRow = { name: string; value: string; secret: boolean };

/** The standard image last used here, for preselection. */
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

const BY_ID = "by-id";

function ImageChoice({
  choice,
  onChoice,
  otherId,
  onOtherId,
}: {
  choice: string | null;
  onChoice: (choice: string) => void;
  otherId: string;
  onOtherId: (id: string) => void;
}) {
  const sets = $api.useQuery("get", "/image-sets");
  const standard = (sets.data ?? []).filter(isStandard);
  const trimmed = otherId.trim();
  const other = $api.useQuery(
    "get",
    "/image-sets/{id}",
    { params: { path: { id: trimmed } } },
    { enabled: choice === BY_ID && isUuid(trimmed), retry: false },
  );

  return (
    <fieldset className="choice-list">
      <legend>Image</legend>
      {sets.isPending && <p className="muted">Loading…</p>}
      <RequestError error={sets.error} />
      {standard.map((s) => (
        <label key={s.id}>
          <input
            type="radio"
            name="image"
            checked={choice === s.id}
            onChange={() => onChoice(s.id)}
          />
          <span>
            <strong>{s.display_name}</strong>
            {s.canonical_name != null && (
              <small className="muted mono">{s.canonical_name}</small>
            )}
          </span>
        </label>
      ))}
      <label>
        <input
          type="radio"
          name="image"
          checked={choice === BY_ID}
          onChange={() => onChoice(BY_ID)}
        />
        <span>
          <strong>Other image</strong>
          {choice === BY_ID && (
            <>
              <input
                aria-label="Image ID"
                className="mono"
                placeholder="Image ID"
                value={otherId}
                onChange={(e) => onOtherId(e.target.value)}
              />
              <small className="muted">
                {!isUuid(trimmed)
                  ? trimmed === ""
                    ? null
                    : "Invalid ID"
                  : other.isPending
                    ? "…"
                    : other.data !== undefined
                      ? other.data.latest_generation == null
                        ? `${other.data.display_name} (no versions)`
                        : `${other.data.display_name} v${other.data.latest_generation}`
                      : "Unknown image"}
              </small>
            </>
          )}
        </span>
      </label>
    </fieldset>
  );
}

type Base = { job: JobInfo; mode: "resume" | "restart" };

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

/** What a resumed or restarted job runs; not changeable here. */
function BaseImage({ base, hostName }: { base: Base; hostName?: string }) {
  const { job, mode } = base;
  return (
    <fieldset className="field" disabled>
      <legend>Image</legend>
      <p className="readonly">
        {mode === "resume" ? "Continues " : "Restarts "}
        <Link to={`/jobs/${job.job_id}`}>
          {job.label ?? <ShortId id={job.job_id} />}
        </Link>
        {mode === "resume" && (
          <>
            {" "}
            on {hostName ?? <ShortId id={job.dispatched_on_host_id ?? ""} />}
          </>
        )}
        {" · "}
        <ImageRef image={job.image} />
      </p>
    </fieldset>
  );
}

function JobForm({ base }: { base: Base | null }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const me = $api.useQuery("get", "/users/me");
  const hosts = $api.useQuery("get", "/hosts", {}, { enabled: base !== null });
  const enqueue = $api.useMutation("post", "/jobs", {
    onSuccess: async (data) => {
      await queryClient.invalidateQueries({ queryKey: ["jobs"] });
      await navigate(`/jobs/${data.job_id}`);
    },
  });

  const [searchParams] = useSearchParams();
  const sets = $api.useQuery("get", "/image-sets");
  const [picked, setPicked] = useState<string | null>(null);
  const [otherId, setOtherId] = useState(searchParams.get("image") ?? "");
  const job = base?.job;
  const [paramRows, setParamRows] = useState<ParamRow[]>(() =>
    Object.entries(job?.parameters ?? {}).map(([name, p]) => ({
      name,
      value: p.value ?? "",
      secret: p.secret,
    })),
  );
  const [owner, setOwner] = useState(
    job?.owner_id == null || job.owner_id === me.data?.user_id
      ? ""
      : job.owner_id,
  );

  // Preselect: the linked image, else the last used, else the first standard.
  const standard = (sets.data ?? []).filter(isStandard);
  const requested = searchParams.get("image");
  const remembered = lastImage();
  const choice =
    picked ??
    (requested !== null
      ? standard.some((s) => s.id === requested)
        ? requested
        : BY_ID
      : sets.data === undefined
        ? null
        : (standard.find((s) => s.id === remembered)?.id ??
          standard[0]?.id ??
          BY_ID));
  const setId = choice === BY_ID ? otherId.trim() : choice;
  const ready = base !== null || (setId !== null && isUuid(setId));

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const str = (k: string): string => {
      const v = f.get(k);
      return typeof v === "string" ? v.trim() : "";
    };

    let init_spec: JobInitSpec;
    if (base !== null) {
      init_spec = { type: base.mode, job_id: base.job.job_id };
    } else {
      if (setId === null || !isUuid(setId)) return;
      if (choice !== BY_ID) rememberImage(setId);
      init_spec = { type: "image_set", set_id: setId, generation: null };
    }
    const parameters: Record<string, JobParameter> = {};
    for (const row of paramRows) {
      if (row.name !== "") {
        parameters[row.name] = { value: row.value, secret: row.secret };
      }
    }

    const leaseDuration = str("lease_duration");
    const leaseExpiryAction = str("lease_expiry_action");
    const label = str("label");
    enqueue.mutate({
      body: {
        init_spec,
        label: label === "" ? null : label,
        host_cel_predicate:
          (job !== undefined && base?.mode === "resume"
            ? job.host_cel_predicate
            : str("host_cel_predicate")) || "true",
        parameters,
        restart_policy: {
          max_restarts:
            base?.mode === "resume" ? 0 : Number(str("max_restarts") || "0"),
        },
        owner: owner === "" ? null : owner,
        lease_duration: leaseDuration === "" ? null : leaseDuration,
        lease_expiry_action:
          leaseExpiryAction === "preempt" ? "preempt" : "terminate",
      },
    });
  }

  return (
    <>
      <h1>
        {base === null ? (
          "Enqueue job"
        ) : (
          <>
            {base.mode === "resume" ? "Resume" : "Restart"}{" "}
            {base.job.label ?? <em>Unnamed Job</em>}{" "}
            <span className="page-id">
              (<ShortId id={base.job.job_id} />)
            </span>
          </>
        )}
      </h1>
      <form className="form" onSubmit={onSubmit}>
        <label className="field">
          <span>Label (optional)</span>
          <input name="label" maxLength={256} defaultValue={job?.label ?? ""} />
        </label>

        {base === null ? (
          <ImageChoice
            choice={choice}
            onChoice={setPicked}
            otherId={otherId}
            onOtherId={setOtherId}
          />
        ) : (
          <BaseImage
            base={base}
            hostName={
              hosts.data?.find(
                (h) => h.host_id === base.job.dispatched_on_host_id,
              )?.name
            }
          />
        )}

        <label className="field">
          <span>
            Host predicate (CEL) — evaluated against the candidate host&rsquo;s
            spec bound as <code>host</code>; empty matches any described host
          </span>
          <input
            name="host_cel_predicate"
            className="mono"
            disabled={base?.mode === "resume"}
            defaultValue={
              job === undefined || job.host_cel_predicate === "true"
                ? ""
                : job.host_cel_predicate
            }
            placeholder="host.site == 'cambridge' &amp;&amp; host.resources.memory_mb >= 4096"
          />
        </label>
        <div className="field">
          <span>Parameters (passed to the puppet daemon)</span>
          {paramRows.map((row, i) => (
            <div className="field-row" key={i}>
              <input
                placeholder="name"
                className="mono"
                value={row.name}
                onChange={(e) =>
                  setParamRows(
                    paramRows.map((r, j) =>
                      j === i ? { ...r, name: e.target.value } : r,
                    ),
                  )
                }
              />
              <input
                placeholder={
                  row.secret && row.value === "" ? "secret: re-enter" : "value"
                }
                required={row.secret && row.value === "" && job !== undefined}
                className="mono"
                value={row.value}
                onChange={(e) =>
                  setParamRows(
                    paramRows.map((r, j) =>
                      j === i ? { ...r, value: e.target.value } : r,
                    ),
                  )
                }
              />
              <label className="check">
                <input
                  type="checkbox"
                  checked={row.secret}
                  onChange={(e) =>
                    setParamRows(
                      paramRows.map((r, j) =>
                        j === i ? { ...r, secret: e.target.checked } : r,
                      ),
                    )
                  }
                />
                secret
              </label>
              <button
                type="button"
                onClick={() =>
                  setParamRows(paramRows.filter((_, j) => j !== i))
                }
              >
                Remove
              </button>
            </div>
          ))}
          <div>
            <button
              type="button"
              onClick={() =>
                setParamRows([
                  ...paramRows,
                  { name: "", value: "", secret: false },
                ])
              }
            >
              Add parameter
            </button>
          </div>
        </div>

        <label className="field">
          <span>
            Max automatic restarts{" "}
            <HelpTip label="About automatic restarts">
              Restarts afresh if the host drops the job. A resumed job never
              restarts.
            </HelpTip>
          </span>
          <input
            name="max_restarts"
            type="number"
            min="0"
            disabled={base?.mode === "resume"}
            defaultValue={
              base?.mode === "resume"
                ? 0
                : (job?.restart_policy.remaining_restarts ?? 0)
            }
          />
        </label>

        <label className="field">
          <span>Lease duration (e.g. “2h”; empty uses the default)</span>
          <input
            name="lease_duration"
            defaultValue={
              job === undefined ? "" : String(job.lease_duration_secs)
            }
          />
        </label>

        <label className="field">
          <span>At lease expiry</span>
          <select
            name="lease_expiry_action"
            defaultValue={job?.lease_expiry_action ?? "terminate"}
          >
            <option value="terminate">Terminate the job</option>
            <option value="preempt">
              Keep running; reclaim when a host is needed
            </option>
          </select>
        </label>

        <label className="field">
          <span>Owner</span>
          <select
            name="owner"
            value={owner}
            onChange={(e) => setOwner(e.target.value)}
          >
            <option value="">{me.data ? `${me.data.name} (me)` : "me"}</option>
            {me.data?.groups.map((g) => (
              <option key={g.group_id} value={g.group_id}>
                group: {g.name}
              </option>
            ))}
          </select>
        </label>

        <RequestError
          error={enqueue.error}
          messages={{
            403: "You are not allowed to start this job.",
            422: "This job can't be resumed.",
            404: "No such image, or it isn't shared with you.",
          }}
        />
        <div className="toolbar">
          <button type="submit" disabled={enqueue.isPending || !ready}>
            {enqueue.isPending ? "Enqueuing…" : "Enqueue"}
          </button>
        </div>
      </form>
    </>
  );
}
