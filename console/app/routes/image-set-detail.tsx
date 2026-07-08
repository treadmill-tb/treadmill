import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import { AuditLog } from "../components/audit-log";
import { Digest } from "../components/digest";
import { EntityLink } from "../components/entity-link";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";
import { Tags } from "../components/tags";
import type { Route } from "./+types/image-set-detail";

type MemberRow = { image_id: string; tags: string };

/// The well-known `everyone` subject (see switchboard `SCHEMA.sql`). Granting it
/// `use` on a set or image source makes the entity public; there is no
/// dedicated public flag.
export const EVERYONE_SUBJECT = "00000000-0000-0000-0000-000000000004";

function NewGenerationForm({
  setId,
  seed,
  onDone,
}: {
  setId: string;
  seed: MemberRow[];
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const [rows, setRows] = useState<MemberRow[]>(seed);
  const create = $api.useMutation("post", "/image-sets/{id}/generations", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-sets/{id}"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-sets/{id}/generations/{n}"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-sets", setId],
        }),
      ]);
      onDone();
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    create.mutate({
      params: { path: { id: setId } },
      body: {
        members: rows
          .filter((r) => r.image_id.trim() !== "")
          .map((r) => ({
            image_id: r.image_id.trim(),
            required_host_tags: r.tags.split(/[\s,]+/).filter((t) => t !== ""),
          })),
      },
    });
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <span className="muted">
        A generation replaces the set's whole membership; earlier members are
        pre-filled. Order is the tie-break among equally-specific members.
      </span>
      {rows.map((row, i) => (
        <div className="field-row" key={i}>
          <input
            placeholder="image id (catalog UUID)"
            className="mono"
            value={row.image_id}
            onChange={(e) =>
              setRows(
                rows.map((r, j) =>
                  j === i ? { ...r, image_id: e.target.value } : r,
                ),
              )
            }
          />
          <input
            placeholder="required host tags"
            className="mono"
            value={row.tags}
            onChange={(e) =>
              setRows(
                rows.map((r, j) =>
                  j === i ? { ...r, tags: e.target.value } : r,
                ),
              )
            }
          />
          <button
            type="button"
            onClick={() => setRows(rows.filter((_, j) => j !== i))}
          >
            Remove
          </button>
        </div>
      ))}
      <div>
        <button
          type="button"
          onClick={() => setRows([...rows, { image_id: "", tags: "" }])}
        >
          Add member
        </button>
      </div>
      <MutationError error={create.error} />
      <div className="toolbar">
        <button type="submit" disabled={create.isPending}>
          {create.isPending ? "Appending…" : "Append generation"}
        </button>
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function GrantForm({ setId, onDone }: { setId: string; onDone: () => void }) {
  const queryClient = useQueryClient();
  const grant = $api.useMutation("post", "/image-sets/{id}/grants", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-sets/{id}/grants"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-sets", setId],
        }),
      ]);
      onDone();
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const subject = f.get("subject_id");
    const permission = f.get("permission");
    if (typeof subject !== "string" || typeof permission !== "string") {
      return;
    }
    grant.mutate({
      params: { path: { id: setId } },
      body: {
        subject_id: subject.trim(),
        permission: permission === "manage" ? "manage" : "use",
      },
    });
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <label className="field">
        <span>Subject id (user or group UUID)</span>
        <input name="subject_id" required className="mono" />
      </label>
      <label className="field">
        <span>Permission</span>
        <select name="permission" defaultValue="use">
          <option value="use">use — may run jobs against the set</option>
          <option value="manage">
            manage — may create generations and manage grants
          </option>
        </select>
      </label>
      <MutationError error={grant.error} />
      <div className="toolbar">
        <button type="submit" disabled={grant.isPending}>
          {grant.isPending ? "Granting…" : "Grant"}
        </button>
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

export default function ImageSetDetail({ params }: Route.ComponentProps) {
  const queryClient = useQueryClient();
  const set = $api.useQuery("get", "/image-sets/{id}", {
    params: { path: { id: params.id } },
  });
  const grants = $api.useQuery("get", "/image-sets/{id}/grants", {
    params: { path: { id: params.id } },
  });

  const latest = set.data?.latest_generation;
  const generation = $api.useQuery(
    "get",
    "/image-sets/{id}/generations/{n}",
    { params: { path: { id: params.id, n: latest ?? 0 } } },
    { enabled: latest != null },
  );

  const [showGenerationForm, setShowGenerationForm] = useState(false);
  const [showGrantForm, setShowGrantForm] = useState(false);

  // "Public" is not a flag on the set: it is a `use` grant to the well-known
  // `everyone` subject. Deriving it needs the grant list, which is manage-gated,
  // so the toggle is only meaningful to a manager (who can read grants).
  const isPublic = grants.data?.some(
    (g) => g.subject_id === EVERYONE_SUBJECT && g.permission === "use",
  );
  const setPublic = $api.useMutation("post", "/image-sets/{id}/grants", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-sets/{id}/grants"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-sets", params.id],
        }),
      ]);
    },
  });
  const revoke = $api.useMutation(
    "delete",
    "/image-sets/{id}/grants/{subject_id}/{permission}",
    {
      onSuccess: async () => {
        await Promise.all([
          queryClient.invalidateQueries({
            queryKey: ["get", "/image-sets/{id}/grants"],
          }),
          queryClient.invalidateQueries({
            queryKey: ["audit", "image-sets", params.id],
          }),
        ]);
      },
    },
  );

  return (
    <>
      {set.isPending && <p className="muted">Loading…</p>}
      {set.isError && <p className="error">Failed to load the set.</p>}
      {set.data && (
        <>
          <div className="toolbar">
            <h1>Set {set.data.name}</h1>
            <span className="spacer" />
            {grants.data && (
              <button
                disabled={setPublic.isPending || revoke.isPending}
                onClick={() => {
                  if (isPublic) {
                    if (
                      window.confirm(
                        "Make this set private again? Revokes the `everyone` grant; only explicit grants keep access.",
                      )
                    ) {
                      revoke.mutate({
                        params: {
                          path: {
                            id: params.id,
                            subject_id: EVERYONE_SUBJECT,
                            permission: "use",
                          },
                        },
                      });
                    }
                  } else if (
                    window.confirm(
                      "Make this set public? Grants `everyone` `use`, so every subject may run jobs against it.",
                    )
                  ) {
                    setPublic.mutate({
                      params: { path: { id: params.id } },
                      body: { subject_id: EVERYONE_SUBJECT, permission: "use" },
                    });
                  }
                }}
              >
                {isPublic ? "Make private" : "Make public"}
              </button>
            )}
            <button onClick={() => setShowGenerationForm(!showGenerationForm)}>
              New generation
            </button>
          </div>
          <MutationError error={setPublic.error} />
          {showGenerationForm && (
            <NewGenerationForm
              setId={params.id}
              seed={
                generation.data?.members.map((m) => ({
                  image_id: m.image_id,
                  tags: m.required_host_tags.join(" "),
                })) ?? []
              }
              onDone={() => setShowGenerationForm(false)}
            />
          )}

          <dl className="props">
            <dt>Id</dt>
            <dd className="mono">{set.data.id}</dd>
            <dt>Label</dt>
            <dd>{set.data.label ?? <span className="muted">—</span>}</dd>
            <dt>Visibility</dt>
            <dd>
              {isPublic == null ? (
                <span className="muted">—</span>
              ) : (
                <span className={`badge ${isPublic ? "warn" : ""}`}>
                  {isPublic ? "public" : "private"}
                </span>
              )}
            </dd>
            <dt>Owner</dt>
            <dd>
              <EntityLink kind="user" id={set.data.owner_id} />
            </dd>
            <dt>Created</dt>
            <dd>
              <RelTime iso={set.data.created_at} />
            </dd>
          </dl>

          <section>
            <h2>
              Latest generation
              {latest != null && (
                <>
                  {" "}
                  <Link
                    to={`/image-sets/${params.id}/generations/${latest}`}
                    className="mono"
                  >
                    #{latest}
                  </Link>
                </>
              )}
            </h2>
            {latest == null && <p className="muted">No generations yet.</p>}
            {latest != null && generation.isPending && (
              <p className="muted">Loading…</p>
            )}
            {generation.isError && (
              <p className="error">Failed to load the generation.</p>
            )}
            {generation.data && (
              <GenerationMembers members={generation.data.members} />
            )}
          </section>

          <section>
            <div className="toolbar">
              <h2>Grants</h2>
              <span className="spacer" />
              <button onClick={() => setShowGrantForm(!showGrantForm)}>
                Grant
              </button>
            </div>
            {showGrantForm && (
              <GrantForm
                setId={params.id}
                onDone={() => setShowGrantForm(false)}
              />
            )}
            <MutationError error={revoke.error} />
            {grants.isPending && <p className="muted">Loading…</p>}
            {grants.isError && (
              <p className="error">Failed to load the grants.</p>
            )}
            {grants.data &&
              (grants.data.length === 0 ? (
                <p className="muted">No explicit grants.</p>
              ) : (
                <table>
                  <thead>
                    <tr>
                      <th>Subject</th>
                      <th>Permission</th>
                      <th></th>
                    </tr>
                  </thead>
                  <tbody>
                    {grants.data.map((grant) => (
                      <tr key={`${grant.subject_id}/${grant.permission}`}>
                        <td>
                          <EntityLink kind="user" id={grant.subject_id} />
                        </td>
                        <td>
                          <span className="badge">{grant.permission}</span>
                        </td>
                        <td>
                          <button
                            className="danger"
                            disabled={revoke.isPending}
                            onClick={() => {
                              if (
                                window.confirm(
                                  `Revoke ${grant.permission} from ${grant.subject_id}?`,
                                )
                              ) {
                                revoke.mutate({
                                  params: {
                                    path: {
                                      id: params.id,
                                      subject_id: grant.subject_id,
                                      permission: grant.permission,
                                    },
                                  },
                                });
                              }
                            }}
                          >
                            Revoke
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              ))}
          </section>

          <AuditLog entity="image-sets" id={params.id} />
        </>
      )}
    </>
  );
}

export function GenerationMembers({
  members,
}: {
  members: {
    index: number;
    image_id: string;
    manifest_digest: string;
    required_host_tags: string[];
    usable: boolean;
    usable_by_grantees: boolean;
  }[];
}) {
  if (members.length === 0) {
    return <p className="muted">No members.</p>;
  }
  return (
    <>
      {members.some((m) => !m.usable_by_grantees) && (
        <p className="error">
          Some members have no source usable by every holder of a `use` grant on
          this set: for those subjects, jobs against this generation will not
          resolve. Grant `use` on a source of the flagged members (e.g. to
          `everyone`) to fix this.
        </p>
      )}
      <table>
        <thead>
          <tr>
            <th>#</th>
            <th>Image</th>
            <th>Digest</th>
            <th>Required host tags</th>
            <th>Usability</th>
          </tr>
        </thead>
        <tbody>
          {members.map((m) => (
            <tr key={m.index}>
              <td>{m.index}</td>
              <td>
                <Link to={`/images/${m.manifest_digest}`} className="mono">
                  {m.image_id.slice(0, 8)}
                </Link>
              </td>
              <td>
                <Digest digest={m.manifest_digest} />
              </td>
              <td>
                <Tags tags={m.required_host_tags} />
              </td>
              <td>
                {!m.usable ? (
                  <span
                    className="badge danger"
                    title="No source of this image is usable by you; a job would not resolve it."
                  >
                    no usable source
                  </span>
                ) : m.usable_by_grantees ? (
                  <span className="badge ok">usable</span>
                ) : (
                  <span
                    className="badge warn"
                    title="Some subject holding a `use` grant on this set cannot use any source of this image."
                  >
                    not grantee-usable
                  </span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}
