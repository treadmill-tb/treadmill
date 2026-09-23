import { ArrowDown, ArrowUp, Pencil, Plus, X } from "lucide-react";
import { useId, useState, type FormEvent } from "react";
import { Link, useNavigate } from "react-router";

import { $api } from "../api/client";
import {
  describeChanges,
  ensureImage,
  parseReference,
  publishVersion,
  type Variant,
} from "../api/images";
import type { components } from "../api/schema";
import { Dialog } from "../components/dialog";
import { HelpTip } from "../components/help-tip";
import { RequestError } from "../components/request-error";
import { VariantList } from "../components/variant-list";
import { Unshareable, useInvalidateImage } from "./image";
import type { Route } from "./+types/image-edit";

type ImageSetInfo = components["schemas"]["ImageSetInfo"];

function useReference(digest: string | undefined): string {
  const image = $api.useQuery(
    "get",
    "/images/{digest}",
    { params: { path: { digest: digest ?? "" } } },
    { enabled: digest !== undefined, retry: false },
  );
  const source = image.data?.sources[0];
  if (digest === undefined) return "";
  return source === undefined
    ? `@${digest}`
    : `${source.registry}/${source.repository}@${digest}`;
}

function VariantDialog({
  initial,
  onClose,
  onSave,
}: {
  initial: Variant | undefined;
  onClose: () => void;
  onSave: (variant: Variant) => void;
}) {
  const initialReference = useReference(initial?.manifest_digest);
  const [reference, setReference] = useState<string | null>(null);
  const [platform, setPlatform] = useState(initial?.platform_profile ?? "");
  const [predicate, setPredicate] = useState(initial?.predicate ?? "");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const hosts = $api.useQuery("get", "/hosts");
  const listId = useId();

  const profiles = [
    ...new Set(
      (hosts.data ?? []).flatMap((h) => h.spec?.platform.profiles ?? []),
    ),
  ].sort();
  const ref = reference ?? initialReference;

  async function onSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const parsed = parseReference(ref);
    if ("error" in parsed) {
      setError(parsed.error);
      return;
    }
    setPending(true);
    setError(null);
    try {
      await ensureImage(parsed.registry, parsed.repository, parsed.digest);
      onSave({
        manifest_digest: parsed.digest,
        platform_profile: platform.trim(),
        predicate: predicate.trim() === "" ? null : predicate.trim(),
      });
    } catch (e) {
      setError(e);
    } finally {
      setPending(false);
    }
  }

  return (
    <Dialog
      open
      onClose={onClose}
      title={initial === undefined ? "Add variant" : "Change variant"}
    >
      <form className="form" onSubmit={(e) => void onSubmit(e)}>
        <label className="field">
          <span>
            Build{" "}
            <HelpTip label="About the build">
              Where to pull it from, including its digest. The switchboard
              checks that it is there.
            </HelpTip>
          </span>
          <input
            required
            className="mono"
            placeholder="ghcr.io/org/repo@sha256:…"
            value={ref}
            onChange={(e) => setReference(e.target.value)}
          />
        </label>
        <label className="field">
          <span>Platform</span>
          <input
            required
            className="mono"
            list={listId}
            placeholder="q35-virtio-uefi"
            value={platform}
            onChange={(e) => setPlatform(e.target.value)}
          />
          <datalist id={listId}>
            {profiles.map((p) => (
              <option key={p} value={p} />
            ))}
          </datalist>
        </label>
        <details className="collapsible" open={predicate !== ""}>
          <summary>Advanced</summary>
          <label className="field">
            <span>
              Only where (CEL){" "}
              <HelpTip label="About conditions">
                Evaluated against the host&rsquo;s spec. Place a variant with a
                condition before the one for the same platform without.
              </HelpTip>
            </span>
            <input
              className="mono"
              placeholder="host.resources.memory_mb >= 16384"
              value={predicate}
              onChange={(e) => setPredicate(e.target.value)}
            />
          </label>
        </details>
        <RequestError
          error={error}
          messages={{
            404: "You can't see that build.",
            422: "Not a Treadmill image.",
            502: "The registry is unreachable, or doesn't have that build.",
          }}
        />
        <div className="toolbar">
          <button type="submit" disabled={pending}>
            {pending ? "Checking…" : initial === undefined ? "Add" : "Save"}
          </button>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
        </div>
      </form>
    </Dialog>
  );
}

function Editor({ set, current }: { set: ImageSetInfo; current: Variant[] }) {
  const navigate = useNavigate();
  const invalidate = useInvalidateImage(set.id);
  const [draft, setDraft] = useState<Variant[]>(current);
  const [editing, setEditing] = useState<number | "new" | null>(null);
  const [blocked, setBlocked] = useState<Variant[] | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);

  const next = (set.latest_generation ?? 0) + 1;
  const changes =
    set.latest_generation == null ? null : describeChanges(current, draft);
  const unchanged = changes === "unchanged";

  function move(i: number, by: number) {
    const j = i + by;
    if (j < 0 || j >= draft.length) return;
    const copy = [...draft];
    const [moved] = copy.splice(i, 1);
    if (moved !== undefined) copy.splice(j, 0, moved);
    setDraft(copy);
  }

  function mark(i: number): string | null {
    const v = draft[i];
    if (v === undefined) return null;
    const same = current.find(
      (c) =>
        c.platform_profile === v.platform_profile &&
        (c.predicate ?? "") === (v.predicate ?? ""),
    );
    if (same === undefined) return "new";
    return same.manifest_digest === v.manifest_digest ? null : "updated";
  }

  async function publish(force: boolean) {
    setPending(true);
    setError(null);
    try {
      const result = await publishVersion(set.id, draft, force);
      if ("blocked" in result) {
        setBlocked(result.blocked);
        return;
      }
      await invalidate();
      await navigate(`/images/${set.id}`);
    } catch (e) {
      setError(e);
    } finally {
      setPending(false);
    }
  }

  return (
    <>
      <header className="page-head">
        <div className="page-head-title">
          <h1>
            <Link to={`/images/${set.id}`}>{set.display_name}</Link> v{next}
          </h1>
          <span className="badge">draft</span>
        </div>
        <div className="page-head-actions">
          <Link className="btn" to={`/images/${set.id}`}>
            Discard
          </Link>
          <button
            type="button"
            className="primary"
            disabled={pending || draft.length === 0 || unchanged}
            onClick={() => void publish(false)}
          >
            {pending ? "Publishing…" : `Publish v${next}`}
          </button>
        </div>
      </header>
      {changes !== null && !unchanged && (
        <p className="page-context">
          <span>Changes: {changes}</span>
        </p>
      )}

      <h2>
        Variants{" "}
        <HelpTip label="About variants">
          A host runs the first variant that matches it. Jobs already started
          keep their version.
        </HelpTip>
      </h2>
      <VariantList
        variants={draft}
        marks={mark}
        actions={(i) => (
          <>
            <button
              type="button"
              className="icon-btn"
              aria-label="Move up"
              disabled={i === 0}
              onClick={() => move(i, -1)}
            >
              <ArrowUp size={16} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-btn"
              aria-label="Move down"
              disabled={i === draft.length - 1}
              onClick={() => move(i, 1)}
            >
              <ArrowDown size={16} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-btn"
              aria-label="Change"
              onClick={() => setEditing(i)}
            >
              <Pencil size={16} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-btn"
              aria-label="Remove"
              onClick={() => setDraft(draft.filter((_, j) => j !== i))}
            >
              <X size={16} aria-hidden="true" />
            </button>
          </>
        )}
      />
      <div className="toolbar">
        <button type="button" onClick={() => setEditing("new")}>
          <Plus size={16} aria-hidden="true" /> Add variant
        </button>
      </div>

      {blocked !== null && (
        <div className="notice">
          <div>
            <Unshareable variants={blocked} />
          </div>
          <button type="button" onClick={() => setBlocked(null)}>
            Keep editing
          </button>
          <button
            type="button"
            className="danger"
            disabled={pending}
            onClick={() => void publish(true)}
          >
            Publish anyway
          </button>
        </div>
      )}
      <RequestError
        error={error}
        messages={{
          403: "You can't publish versions of this image.",
          422: "A variant was rejected; check its condition.",
        }}
      />

      {editing !== null && (
        <VariantDialog
          initial={editing === "new" ? undefined : draft[editing]}
          onClose={() => setEditing(null)}
          onSave={(v) => {
            setDraft(
              editing === "new"
                ? [...draft, v]
                : draft.map((d, j) => (j === editing ? v : d)),
            );
            setEditing(null);
          }}
        />
      )}
    </>
  );
}

export default function ImageEdit({ params }: Route.ComponentProps) {
  const set = $api.useQuery("get", "/image-sets/{id}", {
    params: { path: { id: params.id } },
  });
  const latest = set.data?.latest_generation;
  const version = $api.useQuery(
    "get",
    "/image-sets/{id}/generations/{n}",
    { params: { path: { id: params.id, n: latest ?? 0 } } },
    { enabled: latest != null },
  );

  if (set.isError || version.isError) {
    return (
      <RequestError
        error={set.error ?? version.error}
        messages={{ 404: "No such image, or it isn't shared with you." }}
      />
    );
  }
  if (
    set.data === undefined ||
    (latest != null && version.data === undefined)
  ) {
    return <p className="muted">Loading…</p>;
  }
  const current: Variant[] = (version.data?.members ?? []).map((m) => ({
    manifest_digest: m.manifest_digest,
    platform_profile: m.platform_profile,
    predicate: m.predicate,
  }));
  // Keyed by version: a version published elsewhere starts a fresh draft.
  return <Editor key={latest ?? 0} set={set.data} current={current} />;
}
