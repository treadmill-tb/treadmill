import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useNavigate } from "react-router";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { JsonEditor } from "../components/json-editor";
import { MutationError } from "../components/mutation-error";
import type { Route } from "./+types/host-spec-edit";

type HostInfo = components["schemas"]["HostInfo"];
type HostSpecRejection = components["schemas"]["HostSpecRejection"];
type SpecDocument = components["schemas"]["HostSpecUpdateRequest"]["spec"];

function isSpecDocument(value: unknown): value is SpecDocument {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRejection(error: unknown): error is HostSpecRejection {
  if (typeof error !== "object" || error === null) return false;
  const { path, message } = error as Record<string, unknown>;
  return typeof path === "string" && typeof message === "string";
}

function SpecError({ error }: { error: unknown }) {
  if (!isRejection(error)) {
    return <MutationError error={error} />;
  }
  return (
    <p className="error">
      {error.path === "" ? (
        error.message
      ) : (
        <>
          <code className="mono">{error.path}</code>: {error.message}
        </>
      )}
    </p>
  );
}

function SpecForm({ host }: { host: HostInfo }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const [initialValue] = useState(() =>
    JSON.stringify(
      host.spec ?? { spec_version: "v1", id: host.host_id },
      null,
      2,
    ),
  );
  const [text, setText] = useState(initialValue);
  const [parseError, setParseError] = useState<string | null>(null);

  const validate = $api.useMutation("put", "/hosts/{id}/spec");
  const save = $api.useMutation("put", "/hosts/{id}/spec", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["get", "/hosts/{id}"] }),
        queryClient.invalidateQueries({ queryKey: ["get", "/hosts"] }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "hosts", host.host_id],
        }),
      ]);
      await navigate(`/hosts/${host.host_id}`);
    },
  });

  function submit(validateOnly: boolean) {
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
    setParseError(null);
    const mutation = validateOnly ? validate : save;
    mutation.mutate({
      params: {
        path: { id: host.host_id },
        query: validateOnly ? { validate_only: true } : {},
      },
      body: { spec: parsed },
    });
  }

  const pending = validate.isPending || save.isPending;

  return (
    <>
      <JsonEditor
        initialValue={initialValue}
        onChange={(value) => {
          setText(value);
          setParseError(null);
          validate.reset();
          save.reset();
        }}
      />

      <div className="toolbar">
        <button disabled={pending} onClick={() => submit(true)}>
          {validate.isPending ? "Validating…" : "Validate"}
        </button>
        <button disabled={pending} onClick={() => submit(false)}>
          {save.isPending ? "Saving…" : "Save"}
        </button>
        <Link className="btn" to={`/hosts/${host.host_id}`}>
          Cancel
        </Link>
      </div>

      {parseError !== null && <p className="error">{parseError}</p>}
      <SpecError error={validate.error} />
      <SpecError error={save.error} />
      {validate.isSuccess && (
        <p className="muted">
          The spec is valid. Nothing has been written; press Save to append a
          revision.
        </p>
      )}
    </>
  );
}

export default function HostSpecEdit({ params }: Route.ComponentProps) {
  const host = $api.useQuery("get", "/hosts/{id}", {
    params: { path: { id: params.id } },
  });

  return (
    <>
      <h1>Edit host spec</h1>
      {host.isPending && <p className="muted">Loading…</p>}
      {host.isError && (
        <p className="error">No such host, or you cannot read it.</p>
      )}
      {host.data &&
        (host.data.permissions.includes("manage") ? (
          <>
            <p className="muted">
              {host.data.name}
              {host.data.spec_revision !== null && (
                <> — revision {host.data.spec_revision} is in force</>
              )}
              . Saving appends a revision; every earlier one is kept.
            </p>
            <SpecForm host={host.data} />
          </>
        ) : (
          <p className="error">
            Writing a host&rsquo;s spec needs <code>manage</code> on it.
          </p>
        ))}
    </>
  );
}
