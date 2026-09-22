import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import type { components } from "../api/schema";
import { LiveBadge } from "../components/badges";
import { AuditLog } from "../components/audit-log";
import { EntityLink } from "../components/entity-link";
import { HostSpecView } from "../components/host-spec";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { useResourceWatch } from "../hooks/use-resource-watch";
import type { Route } from "./+types/host-detail";

type HostPermission = components["schemas"]["HostPermission"];

function asHostPermission(value: string): HostPermission {
  return value === "manage" ? "manage" : value === "start" ? "start" : "read";
}

/// Invalidate everything a change to a host's owner or ACL can affect: the host
/// itself (its owner and the viewer's permissions), the listing, its grants and
/// its audit feed.
function useInvalidateHost(hostId: string) {
  const queryClient = useQueryClient();
  return () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: ["get", "/hosts/{id}"] }),
      queryClient.invalidateQueries({ queryKey: ["get", "/hosts"] }),
      queryClient.invalidateQueries({
        queryKey: ["get", "/hosts/{id}/grants"],
      }),
      queryClient.invalidateQueries({ queryKey: ["audit", "hosts", hostId] }),
    ]);
}

function OwnerForm({
  hostId,
  owner,
  onDone,
}: {
  hostId: string;
  owner: string | null | undefined;
  onDone: () => void;
}) {
  const invalidate = useInvalidateHost(hostId);
  const [value, setValue] = useState(owner ?? "");
  const put = $api.useMutation("put", "/hosts/{id}/owner", {
    onSuccess: async () => {
      await invalidate();
      onDone();
    },
  });

  function submit(newOwner: string | null) {
    const message =
      newOwner == null
        ? "Orphan this host? Only global admins will be able to manage it."
        : `Transfer this host to ${newOwner}? Unless you hold a grant on it or are a global admin, you lose access.`;
    if (window.confirm(message)) {
      put.mutate({
        params: { path: { id: hostId } },
        body: { owner: newOwner },
      });
    }
  }

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    submit(value.trim());
  }

  return (
    <form className="form card" onSubmit={onSubmit}>
      <label className="field">
        <span>New owner (user or group UUID)</span>
        <input
          required
          className="mono"
          value={value}
          onChange={(e) => setValue(e.target.value)}
        />
      </label>
      <RequestError
        error={put.error}
        messages={{
          403: "You are not allowed to manage this host.",
          422: "There is no user or group with that ID.",
        }}
      />
      <div className="toolbar">
        <button type="submit" disabled={put.isPending}>
          {put.isPending ? "Transferring…" : "Transfer"}
        </button>
        {owner != null && (
          <button
            type="button"
            className="danger"
            disabled={put.isPending}
            onClick={() => submit(null)}
          >
            Orphan
          </button>
        )}
        <button type="button" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function GrantForm({ hostId, onDone }: { hostId: string; onDone: () => void }) {
  const invalidate = useInvalidateHost(hostId);
  const grant = $api.useMutation("post", "/hosts/{id}/grants", {
    onSuccess: async () => {
      await invalidate();
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
      params: { path: { id: hostId } },
      body: {
        subject_id: subject.trim(),
        permission: asHostPermission(permission),
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
        <select name="permission" defaultValue="start">
          <option value="read">read — may see the host and its spec</option>
          <option value="start">start — may run jobs on the host</option>
          <option value="manage">
            manage — may edit the spec, the owner and the grants
          </option>
        </select>
      </label>
      <RequestError
        error={grant.error}
        messages={{
          403: "You are not allowed to manage this host.",
          422: "There is no user or group with that ID.",
        }}
      />
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

/// The manage-gated grants panel: the host's ACL with revoke buttons, and a
/// grant form. Only mounted for a manager, as the list route is manage-gated.
function HostGrants({ hostId }: { hostId: string }) {
  const invalidate = useInvalidateHost(hostId);
  const grants = $api.useQuery("get", "/hosts/{id}/grants", {
    params: { path: { id: hostId } },
  });
  const [showGrantForm, setShowGrantForm] = useState(false);
  const revoke = $api.useMutation(
    "delete",
    "/hosts/{id}/grants/{subject_id}/{permission}",
    { onSuccess: invalidate },
  );

  return (
    <section>
      <div className="toolbar">
        <h2>Grants</h2>
        <span className="spacer" />
        <button onClick={() => setShowGrantForm(!showGrantForm)}>Grant</button>
      </div>
      {showGrantForm && (
        <GrantForm hostId={hostId} onDone={() => setShowGrantForm(false)} />
      )}
      <RequestError
        error={revoke.error}
        messages={{
          403: "You are not allowed to manage this host.",
          404: "That grant no longer exists.",
          409: "This grant goes with the host and cannot be revoked.",
        }}
      />
      {grants.isPending && <p className="muted">Loading…</p>}
      <RequestError
        error={grants.error}
        messages={{ 403: "Only the host's managers can see its grants." }}
      />
      {grants.data &&
        (grants.data.length === 0 ? (
          <p className="muted">No explicit grants.</p>
        ) : (
          <div className="overflow-auto">
            <table>
              <thead>
                <tr>
                  <th>Subject</th>
                  <th>Permission</th>
                  <th>Granted</th>
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
                      <RelTime iso={grant.granted_at} />
                    </td>
                    <td>
                      {grant.revocable ? (
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
                                    id: hostId,
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
                      ) : (
                        <span
                          className="badge"
                          title="Fixed in place by the switchboard; it goes only with the host."
                        >
                          irrevocable
                        </span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
    </section>
  );
}

export default function HostDetail({ params }: Route.ComponentProps) {
  const host = $api.useQuery("get", "/hosts/{id}", {
    params: { path: { id: params.id } },
  });
  useResourceWatch(`/hosts/${params.id}/watch`, [
    "get",
    "/hosts/{id}",
    { params: { path: { id: params.id } } },
  ]);
  const [showOwnerForm, setShowOwnerForm] = useState(false);

  const canManage = host.data?.permissions.includes("manage") ?? false;

  return (
    <>
      {host.isPending && <p className="muted">Loading…</p>}
      <RequestError
        error={host.error}
        messages={{ 403: "No such host, or you cannot read it." }}
      />
      {host.data && (
        <>
          <h1>
            Host {host.data.name} <LiveBadge live={host.data.live} />
            {host.data.maintenance && (
              <span className="badge warn">maintenance</span>
            )}
          </h1>
          <dl className="props">
            <dt>Id</dt>
            <dd className="mono">{host.data.host_id}</dd>
            <dt>Owner</dt>
            <dd>
              {host.data.owner_id == null ? (
                <span className="muted">orphaned (global admins only)</span>
              ) : (
                <EntityLink kind="user" id={host.data.owner_id} />
              )}
              {canManage && (
                <>
                  {" "}
                  <button onClick={() => setShowOwnerForm(!showOwnerForm)}>
                    Change
                  </button>
                </>
              )}
            </dd>
            <dt>Last seen</dt>
            <dd>
              <RelTime iso={host.data.last_seen_at} />
            </dd>
            <dt>Spec revision</dt>
            <dd>
              {host.data.spec_revision ?? <span className="muted">—</span>}
            </dd>
          </dl>
          {canManage && showOwnerForm && (
            <OwnerForm
              hostId={params.id}
              owner={host.data.owner_id}
              onDone={() => setShowOwnerForm(false)}
            />
          )}

          <section>
            <h2>Spec</h2>
            {canManage && (
              <div className="toolbar">
                <Link className="btn" to={`/hosts/${params.id}/spec`}>
                  {host.data.spec == null ? "Write a spec" : "Edit spec"}
                </Link>
              </div>
            )}
            {host.data.spec == null ? (
              <p className="muted">
                This host has no spec. Nothing can be scheduled onto it: there
                is no description to evaluate a job&rsquo;s predicate against,
                and no platform profile for an image set to match.
              </p>
            ) : (
              <HostSpecView spec={host.data.spec} />
            )}
          </section>

          {canManage && <HostGrants hostId={params.id} />}

          <AuditLog entity="hosts" id={params.id} />
        </>
      )}
    </>
  );
}
