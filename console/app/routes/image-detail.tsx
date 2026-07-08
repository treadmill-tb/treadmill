import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { Digest } from "../components/digest";
import { EntityLink } from "../components/entity-link";
import { MutationError } from "../components/mutation-error";
import { RelTime } from "../components/rel-time";
import { EVERYONE_SUBJECT } from "./image-set-detail";
import type { Route } from "./+types/image-detail";

type ImageSourceInfo = components["schemas"]["ImageSourceInfo"];

function AddSourceForm({
  digest,
  onDone,
}: {
  digest: string;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const add = $api.useMutation("post", "/images/{digest}/sources", {
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: ["get", "/images/{digest}"],
      });
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
    add.mutate({
      params: { path: { digest } },
      body: { registry: str("registry"), repository: str("repository") },
    });
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <span className="muted">
        The switchboard verifies the source serves this image's manifest before
        adding it. You own the source you add.
      </span>
      <label className="field">
        <span>Registry (host:port)</span>
        <input name="registry" required className="mono" />
      </label>
      <label className="field">
        <span>Repository</span>
        <input name="repository" required className="mono" />
      </label>
      <MutationError error={add.error} />
      <div className="toolbar">
        <button type="submit" disabled={add.isPending}>
          {add.isPending ? "Adding…" : "Add source"}
        </button>
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function SourceGrantForm({
  digest,
  sourceId,
  onDone,
}: {
  digest: string;
  sourceId: string;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const grant = $api.useMutation(
    "post",
    "/images/{digest}/sources/{source_id}/grants",
    {
      onSuccess: async () => {
        await Promise.all([
          queryClient.invalidateQueries({
            queryKey: ["get", "/images/{digest}/sources/{source_id}/grants"],
          }),
          queryClient.invalidateQueries({
            queryKey: ["get", "/images/{digest}"],
          }),
        ]);
        onDone();
      },
    },
  );

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const subject = f.get("subject_id");
    const permission = f.get("permission");
    if (typeof subject !== "string" || typeof permission !== "string") {
      return;
    }
    grant.mutate({
      params: { path: { digest, source_id: sourceId } },
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
          <option value="use">
            use — may run jobs that pull the image from this source
          </option>
          <option value="manage">
            manage — may manage grants and delete the source
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

/// The manage-gated grants panel for one source: the grant list with revoke
/// buttons, a grant form, and the public toggle (a `use` grant to the
/// well-known `everyone` subject, like image sets).
function SourceGrants({
  digest,
  source,
}: {
  digest: string;
  source: ImageSourceInfo;
}) {
  const queryClient = useQueryClient();
  const grants = $api.useQuery(
    "get",
    "/images/{digest}/sources/{source_id}/grants",
    { params: { path: { digest, source_id: source.id } } },
  );
  const [showGrantForm, setShowGrantForm] = useState(false);

  const invalidate = async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: ["get", "/images/{digest}/sources/{source_id}/grants"],
      }),
      queryClient.invalidateQueries({
        queryKey: ["get", "/images/{digest}"],
      }),
    ]);
  };
  const setPublic = $api.useMutation(
    "post",
    "/images/{digest}/sources/{source_id}/grants",
    { onSuccess: invalidate },
  );
  const revoke = $api.useMutation(
    "delete",
    "/images/{digest}/sources/{source_id}/grants/{subject_id}/{permission}",
    { onSuccess: invalidate },
  );

  const isPublic = grants.data?.some(
    (g) => g.subject_id === EVERYONE_SUBJECT && g.permission === "use",
  );

  return (
    <div className="card">
      <div className="toolbar">
        <h3>
          Grants on <span className="mono">{source.registry}</span>/
          <span className="mono">{source.repository}</span>
        </h3>
        <span className="spacer" />
        {grants.data && (
          <button
            disabled={setPublic.isPending || revoke.isPending}
            onClick={() => {
              if (isPublic) {
                if (
                  window.confirm(
                    "Make this source private again? Revokes the `everyone` grant; only explicit grants keep access.",
                  )
                ) {
                  revoke.mutate({
                    params: {
                      path: {
                        digest,
                        source_id: source.id,
                        subject_id: EVERYONE_SUBJECT,
                        permission: "use",
                      },
                    },
                  });
                }
              } else if (
                window.confirm(
                  "Make this source public? Grants `everyone` `use`, so every subject may pull the image through it.",
                )
              ) {
                setPublic.mutate({
                  params: { path: { digest, source_id: source.id } },
                  body: { subject_id: EVERYONE_SUBJECT, permission: "use" },
                });
              }
            }}
          >
            {isPublic ? "Make private" : "Make public"}
          </button>
        )}
        <button onClick={() => setShowGrantForm(!showGrantForm)}>Grant</button>
      </div>
      {showGrantForm && (
        <SourceGrantForm
          digest={digest}
          sourceId={source.id}
          onDone={() => setShowGrantForm(false)}
        />
      )}
      <MutationError error={setPublic.error} />
      <MutationError error={revoke.error} />
      {grants.isPending && <p className="muted">Loading…</p>}
      {grants.isError && <p className="error">Failed to load the grants.</p>}
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
                                digest,
                                source_id: source.id,
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
    </div>
  );
}

export default function ImageDetail({ params }: Route.ComponentProps) {
  const queryClient = useQueryClient();
  const image = $api.useQuery("get", "/images/{digest}", {
    params: { path: { digest: params.digest } },
  });

  const [showAddSource, setShowAddSource] = useState(false);
  // The id of the source whose grants panel is expanded, if any.
  const [openGrants, setOpenGrants] = useState<string | null>(null);

  const deleteSource = $api.useMutation(
    "delete",
    "/images/{digest}/sources/{source_id}",
    {
      onSuccess: async () => {
        await queryClient.invalidateQueries({
          queryKey: ["get", "/images/{digest}"],
        });
      },
    },
  );

  const openSource = image.data?.sources.find((s) => s.id === openGrants);

  return (
    <>
      <h1>Image</h1>
      {image.isPending && <p className="muted">Loading…</p>}
      {image.isError && <p className="error">Failed to load the image.</p>}
      {image.data && (
        <>
          <dl className="props">
            <dt>Label</dt>
            <dd>{image.data.label ?? <span className="muted">—</span>}</dd>
            <dt>Id</dt>
            <dd className="mono">{image.data.id}</dd>
            <dt>Digest</dt>
            <dd>
              <Digest digest={image.data.manifest_digest} />
            </dd>
            <dt>Artifact type</dt>
            <dd className="mono">{image.data.artifact_type}</dd>
            <dt>Registered</dt>
            <dd>
              <RelTime iso={image.data.created_at} />
            </dd>
          </dl>

          <section>
            <div className="toolbar">
              <h2>Sources</h2>
              <span className="spacer" />
              <button onClick={() => setShowAddSource(!showAddSource)}>
                Add source
              </button>
            </div>
            {showAddSource && (
              <AddSourceForm
                digest={params.digest}
                onDone={() => setShowAddSource(false)}
              />
            )}
            <MutationError error={deleteSource.error} />
            <table>
              <thead>
                <tr>
                  <th>Registry</th>
                  <th>Repository</th>
                  <th>Status</th>
                  <th>Owner</th>
                  <th>Your permissions</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {image.data.sources.map((src) => (
                  <tr key={src.id}>
                    <td className="mono">{src.registry}</td>
                    <td className="mono">{src.repository}</td>
                    <td>
                      <span className="badge">{src.status}</span>
                    </td>
                    <td>
                      <EntityLink kind="user" id={src.owner_id} />
                    </td>
                    <td>
                      {src.permissions.length === 0 ? (
                        <span className="muted">—</span>
                      ) : (
                        src.permissions.map((p) => (
                          <span key={p} className="badge">
                            {p}
                          </span>
                        ))
                      )}
                    </td>
                    <td>
                      {src.permissions.includes("manage") && (
                        <div className="toolbar">
                          <button
                            onClick={() =>
                              setOpenGrants(
                                openGrants === src.id ? null : src.id,
                              )
                            }
                          >
                            {openGrants === src.id ? "Hide grants" : "Grants"}
                          </button>
                          <button
                            className="danger"
                            disabled={deleteSource.isPending}
                            onClick={() => {
                              if (
                                window.confirm(
                                  `Delete source ${src.registry}/${src.repository}? Subjects relying on it can no longer pull this image through it.`,
                                )
                              ) {
                                deleteSource.mutate({
                                  params: {
                                    path: {
                                      digest: params.digest,
                                      source_id: src.id,
                                    },
                                  },
                                });
                              }
                            }}
                          >
                            Delete
                          </button>
                        </div>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {openSource && (
              <SourceGrants digest={params.digest} source={openSource} />
            )}
          </section>
        </>
      )}
    </>
  );
}
