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
import type { Route } from "./+types/image-group-detail";

type MemberRow = { image_id: string; tags: string };

/// The well-known `everyone` subject (see switchboard `SCHEMA.sql`). Granting it
/// `use` on a group makes the group public; there is no dedicated public flag.
const EVERYONE_SUBJECT = "00000000-0000-0000-0000-000000000004";

function NewGenerationForm({
  groupId,
  seed,
  onDone,
}: {
  groupId: string;
  seed: MemberRow[];
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const [rows, setRows] = useState<MemberRow[]>(seed);
  const create = $api.useMutation("post", "/image-groups/{id}/generations", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-groups/{id}"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-groups/{id}/generations/{n}"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-groups", groupId],
        }),
      ]);
      onDone();
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    create.mutate({
      params: { path: { id: groupId } },
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
        A generation replaces the group's whole membership; earlier members are
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

function GrantForm({
  groupId,
  onDone,
}: {
  groupId: string;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const grant = $api.useMutation("post", "/image-groups/{id}/grants", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-groups/{id}/grants"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-groups", groupId],
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
      params: { path: { id: groupId } },
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
          <option value="use">use — may run jobs against the group</option>
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

export default function ImageGroupDetail({ params }: Route.ComponentProps) {
  const queryClient = useQueryClient();
  const group = $api.useQuery("get", "/image-groups/{id}", {
    params: { path: { id: params.id } },
  });
  const grants = $api.useQuery("get", "/image-groups/{id}/grants", {
    params: { path: { id: params.id } },
  });

  const latest = group.data?.latest_generation;
  const generation = $api.useQuery(
    "get",
    "/image-groups/{id}/generations/{n}",
    { params: { path: { id: params.id, n: latest ?? 0 } } },
    { enabled: latest != null },
  );

  const [showGenerationForm, setShowGenerationForm] = useState(false);
  const [showGrantForm, setShowGrantForm] = useState(false);

  // "Public" is not a flag on the group: it is a `use` grant to the well-known
  // `everyone` subject. Deriving it needs the grant list, which is manage-gated,
  // so the toggle is only meaningful to a manager (who can read grants).
  const isPublic = grants.data?.some(
    (g) => g.subject_id === EVERYONE_SUBJECT && g.permission === "use",
  );
  const setPublic = $api.useMutation("post", "/image-groups/{id}/grants", {
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["get", "/image-groups/{id}/grants"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["audit", "image-groups", params.id],
        }),
      ]);
    },
  });
  const revoke = $api.useMutation(
    "delete",
    "/image-groups/{id}/grants/{subject_id}/{permission}",
    {
      onSuccess: async () => {
        await Promise.all([
          queryClient.invalidateQueries({
            queryKey: ["get", "/image-groups/{id}/grants"],
          }),
          queryClient.invalidateQueries({
            queryKey: ["audit", "image-groups", params.id],
          }),
        ]);
      },
    },
  );

  return (
    <>
      {group.isPending && <p className="muted">Loading…</p>}
      {group.isError && <p className="error">Failed to load the group.</p>}
      {group.data && (
        <>
          <div className="toolbar">
            <h1>Group {group.data.name}</h1>
            <span className="spacer" />
            {grants.data && (
              <button
                disabled={setPublic.isPending || revoke.isPending}
                onClick={() => {
                  if (isPublic) {
                    if (
                      window.confirm(
                        "Make this group private again? Revokes the `everyone` grant; only explicit grants keep access.",
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
                      "Make this group public? Grants `everyone` `use`, so every subject may run jobs against it.",
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
              groupId={params.id}
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
            <dd className="mono">{group.data.id}</dd>
            <dt>Label</dt>
            <dd>{group.data.label ?? <span className="muted">—</span>}</dd>
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
              <EntityLink kind="user" id={group.data.owner_id} />
            </dd>
            <dt>Created</dt>
            <dd>
              <RelTime iso={group.data.created_at} />
            </dd>
          </dl>

          <section>
            <h2>
              Latest generation
              {latest != null && (
                <>
                  {" "}
                  <Link
                    to={`/image-groups/${params.id}/generations/${latest}`}
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
                groupId={params.id}
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

          <AuditLog entity="image-groups" id={params.id} />
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
  }[];
}) {
  if (members.length === 0) {
    return <p className="muted">No members.</p>;
  }
  return (
    <table>
      <thead>
        <tr>
          <th>#</th>
          <th>Image</th>
          <th>Digest</th>
          <th>Required host tags</th>
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
          </tr>
        ))}
      </tbody>
    </table>
  );
}
