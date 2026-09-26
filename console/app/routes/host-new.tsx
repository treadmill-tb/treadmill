import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import { ApiError } from "../api/errors";
import { specVersion } from "../api/hosts";
import type { components } from "../api/schema";
import { JsonEditor } from "../components/json-editor";
import { RequestError } from "../components/request-error";

type HostCreateResponse = components["schemas"]["HostCreateResponse"];
type HostSpecRejection = components["schemas"]["HostSpecRejection"];
type SpecDocument = components["schemas"]["HostCreateRequest"]["spec"];

function isSpecDocument(value: unknown): value is SpecDocument {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRejection(error: unknown): error is HostSpecRejection {
  if (typeof error !== "object" || error === null) return false;
  const { path, message } = error as Record<string, unknown>;
  return typeof path === "string" && typeof message === "string";
}

function CreateError({ error }: { error: unknown }) {
  const rejection = error instanceof ApiError ? error.body : undefined;
  if (!isRejection(rejection)) {
    return (
      <RequestError
        error={error}
        messages={{
          403: "Only global admins can register hosts.",
          409: "A host with this ID already exists.",
        }}
      />
    );
  }
  return (
    <p className="error">
      {rejection.path === "" ? (
        rejection.message
      ) : (
        <>
          <code className="mono">{rejection.path}</code>: {rejection.message}
        </>
      )}
    </p>
  );
}

function template(hostId: string, version: string | undefined): string {
  return JSON.stringify(
    {
      spec_version: version,
      id: hostId,
      name: "",
      description: null,
      site: "",
      location: null,
      platform: {
        kind: "virtual",
        arch: "x86_64",
        profiles: ["q35-virtio-uefi"],
        hypervisor: "qemu",
      },
      resources: { cpu_cores: 4, memory_mb: 8192, storage_gb: 100 },
      labels: {},
      gpio_controllers: {},
      duts: [],
    },
    null,
    2,
  );
}

function Credential({ created }: { created: HostCreateResponse }) {
  return (
    <>
      <div className="card">
        <p>
          <strong>Shown once.</strong> The switchboard never returns this token
          again; a supervisor that loses it needs a new host.
        </p>
        <p className="mono token">{created.auth_token}</p>
        <div className="toolbar">
          <button
            onClick={() =>
              void navigator.clipboard.writeText(created.auth_token)
            }
          >
            Copy token
          </button>
        </div>
      </div>
      <dl className="spec-item">
        <dt>Host id</dt>
        <dd className="mono">{created.host_id}</dd>
        <dt>Spec revision</dt>
        <dd>{created.spec_revision}</dd>
      </dl>
      <div className="toolbar">
        <Link className="btn" to={`/hosts/${created.host_id}`}>
          Open the host
        </Link>
        <Link className="btn" to="/hosts">
          Back to hosts
        </Link>
      </div>
    </>
  );
}

function CreateForm({ version }: { version: string | undefined }) {
  const queryClient = useQueryClient();
  const me = $api.useQuery("get", "/users/me");

  const [hostId] = useState(() => crypto.randomUUID());
  const [initialValue] = useState(() => template(hostId, version));
  const [text, setText] = useState(initialValue);
  const [parseError, setParseError] = useState<string | null>(null);
  const [ownerKind, setOwnerKind] = useState("none");
  const [ownerId, setOwnerId] = useState("");

  const create = $api.useMutation("post", "/hosts", {
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["get", "/hosts"] });
    },
  });

  function owner(): string | null {
    switch (ownerKind) {
      case "none":
        return null;
      case "me":
        return me.data?.user_id ?? null;
      case "other":
        return ownerId.trim() === "" ? null : ownerId.trim();
      default:
        return ownerKind;
    }
  }

  function submit() {
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch (e) {
      setParseError(e instanceof Error ? e.message : String(e));
      return;
    }
    if (!isSpecDocument(parsed)) {
      setParseError("A spec is a JSON object.");
      return;
    }
    if (ownerKind === "me" && me.data === undefined) {
      setParseError("Still loading your profile; try again.");
      return;
    }
    setParseError(null);
    create.mutate({ body: { spec: parsed, owner: owner() } });
  }

  if (create.data) {
    return <Credential created={create.data} />;
  }

  return (
    <>
      <p className="spec-hint">
        The spec&rsquo;s <code className="mono">id</code> becomes the host id.
        One has been generated; keep it, or paste an existing document.
      </p>

      <JsonEditor
        initialValue={initialValue}
        onChange={(value) => {
          setText(value);
          setParseError(null);
          create.reset();
        }}
      />

      <label className="field">
        <span>Owner</span>
        <select
          value={ownerKind}
          onChange={(e) => setOwnerKind(e.target.value)}
        >
          <option value="none">No owner — global admins only</option>
          <option value="me">{me.data ? `${me.data.name} (me)` : "me"}</option>
          {me.data?.groups.map((g) => (
            <option key={g.group_id} value={g.group_id}>
              group: {g.name}
            </option>
          ))}
          <option value="other">Another subject id…</option>
        </select>
      </label>

      {ownerKind === "other" && (
        <label className="field">
          <span>Subject id (user or group UUID)</span>
          <input
            className="mono"
            value={ownerId}
            onChange={(e) => setOwnerId(e.target.value)}
          />
        </label>
      )}

      <div className="toolbar">
        <button
          className="primary"
          disabled={create.isPending}
          onClick={submit}
        >
          {create.isPending ? "Registering…" : "Register"}
        </button>
        <Link className="btn" to="/hosts">
          Cancel
        </Link>
      </div>

      {parseError !== null && <p className="error">{parseError}</p>}
      <CreateError error={create.error} />
    </>
  );
}

export default function HostNew() {
  const whoami = $api.useQuery("get", "/auth/whoami");
  const schema = $api.useQuery("get", "/hosts/spec-schema");

  return (
    <>
      <h1>Register a supervisor</h1>
      {(whoami.isPending || schema.isPending) && (
        <p className="muted">Loading…</p>
      )}
      <RequestError error={whoami.error ?? schema.error} />
      {whoami.data &&
        schema.isSuccess &&
        (whoami.data.admin ? (
          <CreateForm version={specVersion(schema.data)} />
        ) : (
          <p className="error">
            Registering a supervisor mints a credential and puts a machine into
            scheduling: global admins only.
          </p>
        ))}
    </>
  );
}
