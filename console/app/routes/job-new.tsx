import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { MutationError } from "../components/mutation-error";

type JobInitSpec = components["schemas"]["JobInitSpec"];
type JobParameter = components["schemas"]["JobParameter"];

type InitType = JobInitSpec["type"];

type ParamRow = { name: string; value: string; secret: boolean };

export default function JobNew() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const me = $api.useQuery("get", "/users/me");
  const enqueue = $api.useMutation("post", "/jobs", {
    onSuccess: async (data) => {
      await queryClient.invalidateQueries({ queryKey: ["jobs"] });
      await navigate(`/jobs/${data.job_id}`);
    },
  });

  const [initType, setInitType] = useState<InitType>("image");
  const [paramRows, setParamRows] = useState<ParamRow[]>([]);

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const str = (k: string): string => {
      const v = f.get(k);
      return typeof v === "string" ? v.trim() : "";
    };

    let init_spec: JobInitSpec;
    switch (initType) {
      case "image":
        init_spec = { type: "image", manifest_digest: str("manifest_digest") };
        break;
      case "image_set": {
        const gen = str("generation");
        init_spec = {
          type: "image_set",
          set_id: str("set_id"),
          generation: gen === "" ? null : Number(gen),
        };
        break;
      }
      case "resume":
        init_spec = { type: "resume", job_id: str("job_id") };
        break;
      case "restart":
        init_spec = { type: "restart", job_id: str("job_id") };
        break;
    }

    const parameters: Record<string, JobParameter> = {};
    for (const row of paramRows) {
      if (row.name !== "") {
        parameters[row.name] = { value: row.value, secret: row.secret };
      }
    }

    const owner = str("owner");
    const leaseDuration = str("lease_duration");
    const leaseExpiryAction = str("lease_expiry_action");
    const label = str("label");
    enqueue.mutate({
      body: {
        init_spec,
        label: label === "" ? null : label,
        host_cel_predicate: str("host_cel_predicate") || "true",
        parameters,
        restart_policy: { max_restarts: Number(str("max_restarts") || "0") },
        owner: owner === "" ? null : owner,
        lease_duration: leaseDuration === "" ? null : leaseDuration,
        lease_expiry_action:
          leaseExpiryAction === "preempt" ? "preempt" : "terminate",
      },
    });
  }

  return (
    <>
      <h1>Enqueue job</h1>
      <form className="form" onSubmit={onSubmit}>
        <label className="field">
          <span>Label (optional)</span>
          <input name="label" maxLength={256} />
        </label>

        <label className="field">
          <span>Based off</span>
          <select
            value={initType}
            onChange={(e) => setInitType(e.target.value as InitType)}
          >
            <option value="image">Catalog image</option>
            <option value="image_set">Image set</option>
            <option value="resume">Resume a previous job</option>
            <option value="restart">Restart a previous job</option>
          </select>
        </label>

        {initType === "image" && (
          <label className="field">
            <span>Image manifest digest (sha256:…)</span>
            <input name="manifest_digest" required className="mono" />
          </label>
        )}
        {initType === "image_set" && (
          <>
            <label className="field">
              <span>Set id (UUID)</span>
              <input name="set_id" required className="mono" />
            </label>
            <label className="field">
              <span>Generation (empty pins the latest at enqueue)</span>
              <input name="generation" type="number" min="0" />
            </label>
          </>
        )}
        {(initType === "resume" || initType === "restart") && (
          <label className="field">
            <span>Job id (UUID)</span>
            <input name="job_id" required className="mono" />
          </label>
        )}

        <label className="field">
          <span>
            Host predicate (CEL) — evaluated against the candidate host&rsquo;s
            spec bound as <code>host</code>; empty matches any described host
          </span>
          <input
            name="host_cel_predicate"
            className="mono"
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
                placeholder="value"
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
          <span>Max automatic restarts</span>
          <input name="max_restarts" type="number" min="0" defaultValue={0} />
        </label>

        <label className="field">
          <span>Lease duration (e.g. “2h”; empty uses the default)</span>
          <input name="lease_duration" />
        </label>

        <label className="field">
          <span>At lease expiry</span>
          <select name="lease_expiry_action" defaultValue="terminate">
            <option value="terminate">Terminate the job</option>
            <option value="preempt">
              Keep running; reclaim when a host is needed
            </option>
          </select>
        </label>

        <label className="field">
          <span>Owner</span>
          <select name="owner" defaultValue="">
            <option value="">{me.data ? `${me.data.name} (me)` : "me"}</option>
            {me.data?.groups.map((g) => (
              <option key={g.group_id} value={g.group_id}>
                group: {g.name}
              </option>
            ))}
          </select>
        </label>

        <MutationError error={enqueue.error} />
        <div className="toolbar">
          <button type="submit" disabled={enqueue.isPending}>
            {enqueue.isPending ? "Enqueuing…" : "Enqueue"}
          </button>
        </div>
      </form>
    </>
  );
}
