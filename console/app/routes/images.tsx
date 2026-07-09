import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import { Digest } from "../components/digest";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";

function RegisterImageForm({ onDone }: { onDone: () => void }) {
  const queryClient = useQueryClient();
  const register = $api.useMutation("post", "/images/{digest}/sources", {
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["get", "/images"] });
      onDone();
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const str = (k: string): string => {
      const v = f.get(k);
      return typeof v === "string" ? v.trim() : "";
    };
    register.mutate({
      params: { path: { digest: str("manifest_digest") } },
      body: {
        registry: str("registry"),
        repository: str("repository"),
      },
    });
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <label className="field">
        <span>Manifest digest (sha256:…)</span>
        <input name="manifest_digest" required className="mono" />
      </label>
      <label className="field">
        <span>Registry (host:port)</span>
        <input name="registry" required className="mono" />
      </label>
      <label className="field">
        <span>Repository</span>
        <input name="repository" required className="mono" />
      </label>
      <MutationError error={register.error} />
      <div className="toolbar">
        <button type="submit" disabled={register.isPending}>
          {register.isPending ? "Registering…" : "Register"}
        </button>
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

export default function Images() {
  const images = $api.useQuery("get", "/images");
  const [showForm, setShowForm] = useState(false);

  return (
    <>
      <div className="toolbar">
        <h1>Images</h1>
        <span className="spacer" />
        <button onClick={() => setShowForm(!showForm)}>Register image</button>
      </div>
      {showForm && <RegisterImageForm onDone={() => setShowForm(false)} />}
      {images.isPending && <p className="muted">Loading…</p>}
      {images.isError && <p className="error">Failed to load images.</p>}
      {images.data &&
        (images.data.length === 0 ? (
          <p className="muted">No images visible to this account.</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Title</th>
                <th>Digest</th>
                <th>Artifact type</th>
                <th>Sources</th>
                <th>Registered</th>
              </tr>
            </thead>
            <tbody>
              {images.data.map((img) => (
                <tr key={img.manifest_digest}>
                  <td>
                    <Link to={`/images/${img.manifest_digest}`}>
                      {img.title ?? (
                        <span className="mono">
                          {img.manifest_digest.slice(7, 15)}
                        </span>
                      )}
                    </Link>
                  </td>
                  <td>
                    <Digest digest={img.manifest_digest} />
                  </td>
                  <td className="mono">{img.artifact_type}</td>
                  <td>{img.sources.length}</td>
                  <td>
                    <RelTime iso={img.created_at} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ))}
    </>
  );
}
