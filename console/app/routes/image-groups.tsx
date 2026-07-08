import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router";

import { $api } from "../api/client";
import { EntityLink } from "../components/entity-link";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";

function CreateGroupForm({ onDone }: { onDone: () => void }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const create = $api.useMutation("post", "/image-groups", {
    onSuccess: async (data) => {
      await queryClient.invalidateQueries({
        queryKey: ["get", "/image-groups"],
      });
      await navigate(`/image-groups/${data.id}`);
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const str = (k: string): string => {
      const v = f.get(k);
      return typeof v === "string" ? v.trim() : "";
    };
    const label = str("label");
    create.mutate({
      body: {
        name: str("name"),
        label: label === "" ? null : label,
        public: f.get("public") === "on",
      },
    });
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <label className="field">
        <span>Name (stable, globally unique)</span>
        <input name="name" required className="mono" />
      </label>
      <label className="field">
        <span>Label (optional)</span>
        <input name="label" />
      </label>
      <label className="check">
        <input type="checkbox" name="public" />
        Public: every subject may run jobs against this group
      </label>
      <MutationError error={create.error} />
      <div className="toolbar">
        <button type="submit" disabled={create.isPending}>
          {create.isPending ? "Creating…" : "Create"}
        </button>
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

export default function ImageGroups() {
  const groups = $api.useQuery("get", "/image-groups");
  const [showForm, setShowForm] = useState(false);

  return (
    <>
      <div className="toolbar">
        <h1>Image groups</h1>
        <span className="spacer" />
        <button onClick={() => setShowForm(!showForm)}>Create group</button>
      </div>
      {showForm && <CreateGroupForm onDone={() => setShowForm(false)} />}
      {groups.isPending && <p className="muted">Loading…</p>}
      {groups.isError && <p className="error">Failed to load image groups.</p>}
      {groups.data &&
        (groups.data.length === 0 ? (
          <p className="muted">No image groups visible to this account.</p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Label</th>
                <th>Visibility</th>
                <th>Latest generation</th>
                <th>Owner</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {groups.data.map((g) => (
                <tr key={g.id}>
                  <td>
                    <EntityLink kind="imageGroup" id={g.id} label={g.name} />
                  </td>
                  <td>{g.label ?? <span className="muted">—</span>}</td>
                  <td>
                    <span className={`badge ${g.public ? "warn" : ""}`}>
                      {g.public ? "public" : "private"}
                    </span>
                  </td>
                  <td>
                    {g.latest_generation ?? <span className="muted">—</span>}
                  </td>
                  <td>
                    <EntityLink kind="user" id={g.owner_id} />
                  </td>
                  <td>
                    <RelTime iso={g.created_at} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ))}
    </>
  );
}
