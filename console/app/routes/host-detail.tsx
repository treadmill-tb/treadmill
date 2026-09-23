import { useQueryClient } from "@tanstack/react-query";
import { Play, Share2 } from "lucide-react";
import { useState, type FormEvent } from "react";
import { Link } from "react-router";

import { $api, client } from "../api/client";
import { ApiError } from "../api/errors";
import type { components } from "../api/schema";
import { LiveBadge } from "../components/badges";
import { AuditLog } from "../components/audit-log";
import { EntityLink } from "../components/entity-link";
import { HostSpecView } from "../components/host-spec";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import {
  ShareDialog,
  type ApplyAccess,
  type Role,
} from "../components/share-dialog";
import { useResourceWatch } from "../hooks/use-resource-watch";
import type { Route } from "./+types/host-detail";

type HostPermission = components["schemas"]["HostPermission"];

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

const HOST_ROLES: Role<HostPermission>[] = [
  { label: "Can view", detail: "see the host", permissions: ["read"] },
  {
    label: "Can run jobs",
    detail: "also run jobs on it",
    permissions: ["read", "start"],
  },
  {
    label: "Can manage",
    detail: "also change its spec, owner and sharing",
    permissions: ["read", "start", "manage"],
  },
];

const HOST_PUBLIC_LEVELS: Role<HostPermission>[] = [
  {
    label: "Anyone can see it",
    permissions: ["read"],
  },
  {
    label: "Anyone can run jobs on it",
    permissions: ["read", "start"],
  },
];

function HostShareDialog({
  open,
  onClose,
  host,
}: {
  open: boolean;
  onClose: () => void;
  host: { host_id: string; name: string; owner_id?: string | null };
}) {
  const invalidate = useInvalidateHost(host.host_id);
  const grants = $api.useQuery(
    "get",
    "/hosts/{id}/grants",
    { params: { path: { id: host.host_id } } },
    { enabled: open },
  );

  const apply: ApplyAccess<HostPermission> = async (subject, permissions) => {
    const current = (grants.data ?? [])
      .filter((g) => g.subject_id === subject)
      .map((g) => g.permission);
    try {
      for (const permission of permissions) {
        if (current.includes(permission)) continue;
        const r = await client.POST("/hosts/{id}/grants", {
          params: { path: { id: host.host_id } },
          body: { subject_id: subject, permission },
        });
        if (!r.response.ok) throw new ApiError(r.response.status, r.error);
      }
      for (const permission of current) {
        if (permissions.includes(permission)) continue;
        const r = await client.DELETE(
          "/hosts/{id}/grants/{subject_id}/{permission}",
          {
            params: {
              path: { id: host.host_id, subject_id: subject, permission },
            },
          },
        );
        if (!r.response.ok) throw new ApiError(r.response.status, r.error);
      }
    } finally {
      await invalidate();
    }
    return null;
  };

  return (
    <ShareDialog
      open={open}
      onClose={onClose}
      title={host.name}
      ownerId={host.owner_id}
      grants={grants.data}
      grantsError={grants.error}
      roles={HOST_ROLES}
      publicLevels={HOST_PUBLIC_LEVELS}
      apply={apply}
    />
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
  const [sharing, setSharing] = useState(false);

  const canManage = host.data?.permissions.includes("manage") ?? false;
  const canStart = host.data?.permissions.includes("start") ?? false;

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
            )}{" "}
            {canManage && (
              <button type="button" onClick={() => setSharing(true)}>
                <Share2 size={14} aria-hidden="true" /> Share
              </button>
            )}{" "}
            {canStart && (
              <Link
                className="btn primary"
                to={`/jobs/new?host=${host.data.host_id}`}
              >
                <Play size={14} aria-hidden="true" /> Run job here
              </Link>
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

          {canManage && (
            <HostShareDialog
              open={sharing}
              onClose={() => setSharing(false)}
              host={host.data}
            />
          )}

          <AuditLog entity="hosts" id={params.id} />
        </>
      )}
    </>
  );
}
