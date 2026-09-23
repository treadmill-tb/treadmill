import { useQueries, useQueryClient } from "@tanstack/react-query";
import {
  Globe,
  Lock,
  Pencil,
  Play,
  Share2,
  Star,
  TriangleAlert,
} from "lucide-react";
import { useState, type FormEvent } from "react";
import { Link } from "react-router";

import { $api } from "../api/client";
import { ApiError } from "../api/errors";
import {
  describeChanges,
  fixAccess,
  isStandard,
  publishVersion,
  type Variant,
} from "../api/images";
import type { components } from "../api/schema";
import { EVERYONE_SUBJECT, SYSTEM_SUBJECT } from "../api/subjects";
import { AuditLog } from "../components/audit-log";
import { ConfirmDialog, Dialog } from "../components/dialog";
import { HelpTip } from "../components/help-tip";
import { ImageShareDialog } from "../components/image-share";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { SubjectName } from "../components/subject";
import { VariantList } from "../components/variant-list";
import { CanonicalNameField } from "./images";
import type { Route } from "./+types/image";

type ImageSetInfo = components["schemas"]["ImageSetInfo"];
type ImageSetGenerationInfo = components["schemas"]["ImageSetGenerationInfo"];

const HISTORY_PAGE = 10;

export function useInvalidateImage(setId: string) {
  const queryClient = useQueryClient();
  return () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: ["get", "/image-sets"] }),
      queryClient.invalidateQueries({ queryKey: ["get", "/image-sets/{id}"] }),
      queryClient.invalidateQueries({
        queryKey: ["get", "/image-sets/{id}/generations/{n}"],
      }),
      queryClient.invalidateQueries({
        queryKey: ["get", "/image-sets/{id}/grants"],
      }),
      queryClient.invalidateQueries({
        queryKey: ["audit", "image-sets", setId],
      }),
    ]);
}

export function RestoreButton({
  setId,
  version,
}: {
  setId: string;
  version: ImageSetGenerationInfo;
}) {
  const invalidate = useInvalidateImage(setId);
  const [confirming, setConfirming] = useState(false);
  const [blocked, setBlocked] = useState<Variant[] | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);

  async function restore(force: boolean) {
    setPending(true);
    setError(null);
    try {
      const result = await publishVersion(setId, version.members, force);
      if ("blocked" in result) {
        setBlocked(result.blocked);
        return;
      }
      setConfirming(false);
      setBlocked(null);
      await invalidate();
    } catch (e) {
      setError(e);
    } finally {
      setPending(false);
    }
  }

  return (
    <>
      <button type="button" onClick={() => setConfirming(true)}>
        Restore
      </button>
      <ConfirmDialog
        open={confirming}
        title={`Restore v${version.generation}?`}
        confirmLabel={
          pending ? "Restoring…" : blocked ? "Restore anyway" : "Restore"
        }
        danger={blocked !== null}
        onConfirm={() => void restore(blocked !== null)}
        onCancel={() => {
          setConfirming(false);
          setBlocked(null);
        }}
      >
        <p>Published as a new version.</p>
        {blocked !== null && <Unshareable variants={blocked} />}
        <RequestError error={error} />
      </ConfirmDialog>
    </>
  );
}

/** Variants whose builds the viewer can't share. */
export function Unshareable({ variants }: { variants: Variant[] }) {
  return (
    <p className="error">
      Can&rsquo;t share: {variants.map((v) => v.platform_profile).join(", ")}{" "}
      <HelpTip label="Why">
        You don&rsquo;t manage where these builds are stored. Unless their
        owners have shared them, people this image is shared with can&rsquo;t
        run it on these platforms.
      </HelpTip>
    </p>
  );
}

function History({ set }: { set: ImageSetInfo }) {
  const latest = set.latest_generation ?? 0;
  const [shown, setShown] = useState(HISTORY_PAGE);
  // One extra, to diff the oldest shown version against.
  const numbers: number[] = [];
  for (let n = latest; n >= 1 && numbers.length <= shown; n--) numbers.push(n);
  const versions = useQueries({
    queries: numbers.map((n) =>
      $api.queryOptions("get", "/image-sets/{id}/generations/{n}", {
        params: { path: { id: set.id, n } },
      }),
    ),
  });
  const grants = $api.useQuery(
    "get",
    "/image-sets/{id}/grants",
    { params: { path: { id: set.id } } },
    { retry: false },
  );
  const canManage = grants.isSuccess;

  if (set.latest_generation == null) return null;
  return (
    <section>
      <h2>History</h2>
      <ol className="version-list">
        {numbers.slice(0, shown).map((n, i) => {
          const version = versions[i]?.data;
          const previous = versions[i + 1]?.data;
          return (
            <li key={n}>
              <Link to={`/images/${set.id}/versions/${n}`}>v{n}</Link>
              {version === undefined ? (
                <span className="muted">Loading…</span>
              ) : (
                <>
                  <span className="muted">
                    <RelTime iso={version.created_at} />
                  </span>
                  <span>
                    {version.created_by != null && (
                      <SubjectName id={version.created_by} />
                    )}
                  </span>
                  <span className="version-changes">
                    {describeChanges(
                      n === 1 ? undefined : previous?.members,
                      version.members,
                    )}
                  </span>
                  {n === latest ? (
                    <span className="badge ok">current</span>
                  ) : (
                    canManage && (
                      <RestoreButton setId={set.id} version={version} />
                    )
                  )}
                </>
              )}
            </li>
          );
        })}
      </ol>
      {latest > shown && (
        <button type="button" onClick={() => setShown(shown + HISTORY_PAGE)}>
          Show older
        </button>
      )}
    </section>
  );
}

function RenameDialog({
  onClose,
  set,
}: {
  onClose: () => void;
  set: ImageSetInfo;
}) {
  const invalidate = useInvalidateImage(set.id);
  const whoami = $api.useQuery("get", "/auth/whoami");
  const admin = whoami.data?.admin === true;
  const patch = $api.useMutation("patch", "/image-sets/{id}", {
    onSuccess: async () => {
      await invalidate();
      onClose();
    },
  });

  function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    const str = (k: string): string => {
      const v = f.get(k);
      return typeof v === "string" ? v.trim() : "";
    };
    const canonical = str("canonical_name");
    patch.mutate({
      params: { path: { id: set.id } },
      body: {
        display_name: str("display_name"),
        ...(admin && canonical !== (set.canonical_name ?? "")
          ? { canonical_name: canonical === "" ? null : canonical }
          : {}),
      },
    });
  }

  return (
    <Dialog open onClose={onClose} title="Rename">
      <form className="form" onSubmit={onSubmit}>
        <label className="field">
          <span>Name</span>
          <input name="display_name" required defaultValue={set.display_name} />
        </label>
        {admin && (
          <CanonicalNameField defaultValue={set.canonical_name ?? ""} />
        )}
        <RequestError
          error={patch.error}
          messages={{ 409: "That canonical name is taken." }}
        />
        <div className="toolbar">
          <button type="submit" disabled={patch.isPending}>
            {patch.isPending ? "Saving…" : "Save"}
          </button>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
        </div>
      </form>
    </Dialog>
  );
}

const ME = "me";

function OwnerDialog({
  onClose,
  set,
}: {
  onClose: () => void;
  set: ImageSetInfo;
}) {
  const invalidate = useInvalidateImage(set.id);
  const whoami = $api.useQuery("get", "/auth/whoami");
  const me = $api.useQuery("get", "/users/me");
  const [choice, setChoice] = useState<string>(set.owner_id ?? ME);
  const put = $api.useMutation("put", "/image-sets/{id}/owner", {
    onSuccess: async () => {
      await invalidate();
      onClose();
    },
  });
  const myId = me.data?.user_id;
  const owner = choice === ME ? myId : choice;

  return (
    <Dialog
      open
      onClose={onClose}
      title="Owner"
      footer={
        <>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            disabled={
              put.isPending || owner === undefined || owner === set.owner_id
            }
            onClick={() =>
              owner !== undefined &&
              put.mutate({
                params: { path: { id: set.id } },
                body: { owner },
              })
            }
          >
            {put.isPending ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <fieldset className="choice-list">
        {whoami.data?.admin && (
          <label>
            <input
              type="radio"
              name="owner"
              checked={choice === SYSTEM_SUBJECT}
              onChange={() => setChoice(SYSTEM_SUBJECT)}
            />
            <span>
              <strong>
                Treadmill (standard image){" "}
                <HelpTip label="About standard images">
                  Shown to everyone and preselected for new jobs. Make it public
                  too.
                </HelpTip>
              </strong>
            </span>
          </label>
        )}
        <label>
          <input
            type="radio"
            name="owner"
            checked={choice === ME || choice === myId}
            onChange={() => setChoice(ME)}
          />
          <span>
            <strong>{me.data ? `${me.data.name} (you)` : "You"}</strong>
          </span>
        </label>
        {me.data?.groups
          .filter((g) => g.group_id !== EVERYONE_SUBJECT)
          .map((g) => (
            <label key={g.group_id}>
              <input
                type="radio"
                name="owner"
                checked={choice === g.group_id}
                onChange={() => setChoice(g.group_id)}
              />
              <span>
                <strong>Group: {g.name}</strong>
              </span>
            </label>
          ))}
      </fieldset>
      <RequestError
        error={put.error}
        messages={{
          403: "You can't give this image to that owner.",
          422: "That can't own an image.",
        }}
      />
    </Dialog>
  );
}

function AccessNotice({
  set,
  version,
}: {
  set: ImageSetInfo;
  version: ImageSetGenerationInfo;
}) {
  const invalidate = useInvalidateImage(set.id);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [unfixable, setUnfixable] = useState<Variant[] | null>(null);
  const broken = version.members.filter((m) => !m.usable_by_grantees);
  if (broken.length === 0) return null;

  async function fix() {
    setPending(true);
    setError(null);
    try {
      setUnfixable(await fixAccess(set.id, version.members));
      await invalidate();
    } catch (e) {
      setError(e);
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="notice">
      <TriangleAlert size={18} aria-hidden="true" />
      <div>
        <strong>
          Not shared with everyone:{" "}
          {[...new Set(broken.map((m) => m.platform_profile))].join(", ")}
        </strong>{" "}
        <HelpTip label="Why">
          Some people this image is shared with can&rsquo;t pull these builds,
          so their jobs won&rsquo;t run on these platforms.
        </HelpTip>
        {unfixable !== null && unfixable.length > 0 && (
          <Unshareable variants={unfixable} />
        )}
        <RequestError error={error} />
      </div>
      <button type="button" disabled={pending} onClick={() => void fix()}>
        {pending ? "Fixing…" : "Fix access"}
      </button>
    </div>
  );
}

export default function Image({ params }: Route.ComponentProps) {
  const set = $api.useQuery("get", "/image-sets/{id}", {
    params: { path: { id: params.id } },
  });
  // Only managers may read grants; this doubles as the manage check.
  const grants = $api.useQuery(
    "get",
    "/image-sets/{id}/grants",
    { params: { path: { id: params.id } } },
    { retry: false },
  );
  const latest = set.data?.latest_generation;
  const version = $api.useQuery(
    "get",
    "/image-sets/{id}/generations/{n}",
    { params: { path: { id: params.id, n: latest ?? 0 } } },
    { enabled: latest != null },
  );
  const [dialog, setDialog] = useState<"share" | "owner" | "rename" | null>(
    null,
  );

  if (set.isError) {
    return (
      <RequestError
        error={set.error}
        messages={{ 404: "No such image, or it isn't shared with you." }}
      />
    );
  }
  if (set.data === undefined) return <p className="muted">Loading…</p>;

  const data = set.data;
  const canManage = grants.isSuccess;
  const isPublic = grants.data?.some(
    (g) => g.subject_id === EVERYONE_SUBJECT && g.permission === "use",
  );
  const grantsError: unknown = grants.error;
  const forbidden =
    grantsError instanceof ApiError && grantsError.status === 403;

  return (
    <>
      <header className="page-head">
        <div className="page-head-title">
          <h1>{data.display_name}</h1>
          {canManage && (
            <button
              type="button"
              className="icon-btn"
              title="Rename"
              aria-label="Rename"
              onClick={() => setDialog("rename")}
            >
              <Pencil size={18} aria-hidden="true" />
            </button>
          )}
          {isStandard(data) && (
            <span className="badge ok">
              <Star size={12} aria-hidden="true" /> Standard
            </span>
          )}
          {isPublic === true && (
            <span className="badge">
              <Globe size={12} aria-hidden="true" /> Public
            </span>
          )}
          {isPublic === false && (
            <span className="badge">
              <Lock size={12} aria-hidden="true" /> Private
            </span>
          )}
        </div>
        <div className="page-head-actions">
          {canManage && (
            <>
              <button type="button" onClick={() => setDialog("share")}>
                <Share2 size={14} aria-hidden="true" /> Share
              </button>
              <Link className="btn" to={`/images/${data.id}/edit`}>
                <Pencil size={14} aria-hidden="true" /> Edit variants
              </Link>
            </>
          )}
          {latest != null && (
            <Link className="btn primary" to={`/jobs/new?image=${data.id}`}>
              <Play size={14} aria-hidden="true" /> Run job
            </Link>
          )}
        </div>
      </header>
      <p className="page-context">
        {data.canonical_name != null && (
          <span className="mono">{data.canonical_name}</span>
        )}
        <span>
          Owner:{" "}
          {data.owner_id == null ? (
            <span className="muted">none</span>
          ) : (
            <SubjectName id={data.owner_id} />
          )}
          {canManage && (
            <button
              type="button"
              className="link-btn"
              onClick={() => setDialog("owner")}
            >
              change
            </button>
          )}
        </span>
        {latest != null && version.data !== undefined && (
          <span>
            v{latest}, <RelTime iso={version.data.created_at} />
          </span>
        )}
      </p>
      {!canManage && !forbidden && <RequestError error={grants.error} />}

      {canManage && version.data !== undefined && (
        <AccessNotice set={data} version={version.data} />
      )}

      <section>
        <h2>
          Variants{" "}
          <HelpTip label="About variants">
            One build per host platform. A host runs the first variant that
            matches it.
          </HelpTip>
        </h2>
        {latest == null ? (
          <p className="muted">None</p>
        ) : (
          <>
            {version.isPending && <p className="muted">Loading…</p>}
            <RequestError error={version.error} />
            {version.data !== undefined && (
              <VariantList
                variants={version.data.members}
                showAccess={canManage}
              />
            )}
          </>
        )}
      </section>

      <History set={data} />

      {canManage && <AuditLog entity="image-sets" id={data.id} />}

      <ImageShareDialog
        open={dialog === "share"}
        onClose={() => setDialog(null)}
        setId={data.id}
        title={data.display_name}
        ownerId={data.owner_id}
        grants={grants.data}
        grantsError={grants.error}
        variants={version.data?.members ?? []}
      />
      {dialog === "owner" && (
        <OwnerDialog onClose={() => setDialog(null)} set={data} />
      )}
      {dialog === "rename" && (
        <RenameDialog onClose={() => setDialog(null)} set={data} />
      )}
    </>
  );
}
