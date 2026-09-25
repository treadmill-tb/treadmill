import { useQueryClient } from "@tanstack/react-query";
import { Pencil, Play, RotateCcw, Share2, Wrench } from "lucide-react";
import { useState, type FormEvent } from "react";
import { Link } from "react-router";

import { $api, client } from "../api/client";
import { ApiError } from "../api/errors";
import type { HostSpecV2 } from "../api/host-spec";
import type { components } from "../api/schema";
import { LiveBadge } from "../components/badges";
import { AuditLog } from "../components/audit-log";
import { DutCard } from "../components/dut-card";
import { HostTopology } from "../components/host-topology";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { SubjectLink } from "../components/subject";
import {
  ShareDialog,
  type ApplyAccess,
  type Role,
} from "../components/share-dialog";
import { useResourceWatch } from "../hooks/use-resource-watch";
import { PlatformIcon } from "../icons";
import type { Route } from "./+types/host-detail";

type HostPermission = components["schemas"]["HostPermission"];

function formatMemory(mb: number): string {
  const gb = mb / 1024;
  return `${gb < 10 ? gb.toFixed(1).replace(/\.0$/, "") : Math.round(gb)} GB`;
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
  host: { host_id: string; name: string; owner?: { id: string } | null };
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
      ownerId={host.owner?.id}
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
  const invalidate = useInvalidateHost(params.id);
  const patch = $api.useMutation("patch", "/hosts/{id}", {
    onSuccess: invalidate,
  });

  function setMaintenance(maintenance: boolean) {
    const message = maintenance
      ? "Put this host into maintenance?"
      : "Resume this host?";
    if (window.confirm(message)) {
      patch.mutate({
        params: { path: { id: params.id } },
        body: { maintenance },
      });
    }
  }

  const spec = host.data?.spec as HostSpecV2 | null | undefined;
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
          <div className="toolbar">
            <h1 className="host-name">
              {spec != null && (
                <PlatformIcon
                  platform={spec.platform}
                  size={28}
                  aria-hidden="true"
                />
              )}
              {host.data.name}
              <LiveBadge live={host.data.live} />
              {host.data.live && host.data.busy && (
                <span className="badge warn">busy</span>
              )}
              {host.data.maintenance && (
                <span className="badge warn">maintenance</span>
              )}
            </h1>
            <span className="spacer" />
            {canStart && (
              <Link
                className="btn primary"
                to={`/jobs/new?host=${host.data.host_id}`}
              >
                <Play size={14} aria-hidden="true" /> Run job
              </Link>
            )}
            {canManage && (
              <button
                type="button"
                disabled={patch.isPending}
                onClick={() => setMaintenance(!host.data.maintenance)}
              >
                {host.data.maintenance ? (
                  <>
                    <RotateCcw size={14} aria-hidden="true" /> Resume
                  </>
                ) : (
                  <>
                    <Wrench size={14} aria-hidden="true" /> Maintenance
                  </>
                )}
              </button>
            )}
            {canManage && (
              <button type="button" onClick={() => setSharing(true)}>
                <Share2 size={14} aria-hidden="true" /> Share
              </button>
            )}
            {canManage && (
              <Link className="btn" to={`/hosts/${params.id}/spec`}>
                <Pencil size={14} aria-hidden="true" />{" "}
                {spec == null ? "Write spec" : "Edit spec"}
              </Link>
            )}
          </div>
          <RequestError
            error={patch.error}
            messages={{ 403: "You are not allowed to manage this host." }}
          />
          <section className="card host-overview">
            {spec?.description != null && (
              <p className="muted">{spec.description}</p>
            )}
            <div className="host-overview-columns">
              {spec != null && (
                <dl className="props">
                  <dt>Platform</dt>
                  <dd>
                    {spec.platform.kind === "physical"
                      ? `${spec.platform.vendor} · ${spec.platform.model}`
                      : `${spec.platform.hypervisor} (virtual)`}
                  </dd>
                  <dt>Architecture</dt>
                  <dd className="mono">{spec.platform.arch}</dd>
                  <dt>Resources</dt>
                  <dd>
                    {spec.resources.cpu_cores} cores,{" "}
                    {formatMemory(spec.resources.memory_mb)} memory,{" "}
                    {spec.resources.storage_gb} GB storage
                  </dd>
                  <dt>Profiles</dt>
                  <dd>
                    {spec.platform.profiles.map((p) => (
                      <span key={p} className="chip mono">
                        {p}
                      </span>
                    ))}
                  </dd>
                </dl>
              )}
              <dl className="props">
                {spec != null && (
                  <>
                    <dt>Site</dt>
                    <dd>{spec.site}</dd>
                    {spec.location != null && (
                      <>
                        <dt>Location</dt>
                        <dd>{spec.location}</dd>
                      </>
                    )}
                    {Object.keys(spec.labels).length > 0 && (
                      <>
                        <dt>Labels</dt>
                        <dd>
                          {Object.entries(spec.labels).map(([key, value]) => (
                            <span key={key} className="chip mono">
                              {key}={value}
                            </span>
                          ))}
                        </dd>
                      </>
                    )}
                  </>
                )}
                <dt>Owner</dt>
                <dd>
                  {host.data.owner == null ? (
                    <span className="muted">orphaned</span>
                  ) : (
                    <SubjectLink subject={host.data.owner} />
                  )}
                  {canManage && (
                    <button
                      type="button"
                      className="icon-btn"
                      aria-label="Change owner"
                      onClick={() => setShowOwnerForm(!showOwnerForm)}
                    >
                      <Pencil size={14} />
                    </button>
                  )}
                </dd>
                <dt>Last seen</dt>
                <dd>
                  {host.data.last_seen_at == null ? (
                    <span className="muted">never</span>
                  ) : (
                    <RelTime iso={host.data.last_seen_at} />
                  )}
                </dd>
                <dt>Spec revision</dt>
                <dd>
                  {host.data.spec_revision ?? (
                    <span className="muted">none</span>
                  )}
                </dd>
              </dl>
            </div>
          </section>
          {canManage && showOwnerForm && (
            <OwnerForm
              hostId={params.id}
              owner={host.data.owner?.id}
              onDone={() => setShowOwnerForm(false)}
            />
          )}

          {spec == null ? (
            <p className="muted">No spec. Nothing can be scheduled here.</p>
          ) : (
            <>
              {(spec.duts.length > 0 ||
                Object.keys(spec.gpio_controllers).length > 0) && (
                <section>
                  <h2>Topology</h2>
                  <HostTopology spec={spec} />
                </section>
              )}
              {spec.duts.length > 0 && (
                <section>
                  <h2>Devices</h2>
                  <div className="dut-cards">
                    {spec.duts.map((dut, i) => (
                      <DutCard key={i} dut={dut} />
                    ))}
                  </div>
                </section>
              )}
              <section>
                <details className="collapsible">
                  <summary>
                    <h2>Raw spec</h2>
                  </summary>
                  <pre className="raw-spec">
                    <code>{JSON.stringify(spec, null, 2)}</code>
                  </pre>
                </details>
              </section>
            </>
          )}

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
