import { useQueries } from "@tanstack/react-query";
import { Play, Plus } from "lucide-react";
import { useState, type FormEvent } from "react";
import { Link, useNavigate } from "react-router";

import { $api } from "../api/client";
import { isStandard } from "../api/images";
import type { components } from "../api/schema";
import { Dialog } from "../components/dialog";
import { HelpTip } from "../components/help-tip";
import { RelTime } from "../components/rel-time";
import { RequestError } from "../components/request-error";
import { SubjectLink } from "../components/subject";

type ImageSetInfo = components["schemas"]["ImageSetInfo"];
type ImageSetGenerationInfo = components["schemas"]["ImageSetGenerationInfo"];

const OTHERS_SHOWN = 10;

function NewImageDialog({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const navigate = useNavigate();
  const whoami = $api.useQuery("get", "/auth/whoami");
  const create = $api.useMutation("post", "/image-sets", {
    onSuccess: async (data) => {
      await navigate(`/images/${data.id}/edit`);
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
    create.mutate({
      body: {
        display_name: str("display_name"),
        canonical_name: canonical === "" ? null : canonical,
      },
    });
  }

  return (
    <Dialog open={open} onClose={onClose} title="New image">
      <form className="form" onSubmit={onSubmit}>
        <label className="field">
          <span>Name</span>
          <input name="display_name" required />
        </label>
        {whoami.data?.admin && <CanonicalNameField defaultValue="" />}
        <RequestError
          error={create.error}
          messages={{ 409: "That canonical name is taken." }}
        />
        <div className="toolbar">
          <button type="submit" disabled={create.isPending}>
            {create.isPending ? "Creating…" : "Create"}
          </button>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
        </div>
      </form>
    </Dialog>
  );
}

export function CanonicalNameField({ defaultValue }: { defaultValue: string }) {
  return (
    <label className="field">
      <span>
        Canonical name (optional){" "}
        <HelpTip label="About canonical names">
          A unique handle, such as <code>linux</code>. Only admins can set it.
        </HelpTip>
      </span>
      <input
        name="canonical_name"
        className="mono"
        defaultValue={defaultValue}
      />
    </label>
  );
}

function Platforms({
  version,
}: {
  version: ImageSetGenerationInfo | undefined;
}) {
  if (version === undefined) return null;
  const platforms = [
    ...new Set(version.members.map((m) => m.platform_profile)),
  ];
  if (platforms.length === 0) return <span className="muted">—</span>;
  return (
    <>
      {platforms.map((p) => (
        <span key={p} className="chip mono">
          {p}
        </span>
      ))}
    </>
  );
}

function Version({
  set,
  version,
}: {
  set: ImageSetInfo;
  version: ImageSetGenerationInfo | undefined;
}) {
  if (set.latest_generation == null) {
    return <span className="muted">—</span>;
  }
  return (
    <span>
      v{set.latest_generation}
      {version !== undefined && (
        <>
          {" · "}
          <RelTime iso={version.created_at} />
        </>
      )}
    </span>
  );
}

function ImageRows({
  sets,
  versions,
  showOwner,
}: {
  sets: ImageSetInfo[];
  versions: Map<string, ImageSetGenerationInfo>;
  showOwner: boolean;
}) {
  return (
    <div className="overflow-auto">
      <table>
        <thead>
          <tr>
            <th>Image</th>
            <th>Platforms</th>
            <th>Version</th>
            {showOwner && <th>Owner</th>}
          </tr>
        </thead>
        <tbody>
          {sets.map((s) => (
            <tr key={s.id}>
              <td>
                <Link to={`/images/${s.id}`}>{s.display_name}</Link>
                {s.canonical_name != null && (
                  <div className="muted mono">{s.canonical_name}</div>
                )}
              </td>
              <td>
                <Platforms version={versions.get(s.id)} />
              </td>
              <td>
                <Version set={s} version={versions.get(s.id)} />
              </td>
              {showOwner && (
                <td>
                  {s.owner == null ? (
                    <span className="muted">—</span>
                  ) : (
                    <SubjectLink subject={s.owner} />
                  )}
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export default function Images() {
  const sets = $api.useQuery("get", "/image-sets");
  const me = $api.useQuery("get", "/users/me");
  const [query, setQuery] = useState("");
  const [creating, setCreating] = useState(false);
  const [showAllOthers, setShowAllOthers] = useState(false);

  const withVersions = (sets.data ?? []).filter(
    (s) => s.latest_generation != null,
  );
  const versionQueries = useQueries({
    queries: withVersions.map((s) =>
      $api.queryOptions("get", "/image-sets/{id}/generations/{n}", {
        params: { path: { id: s.id, n: s.latest_generation ?? 0 } },
      }),
    ),
  });
  const versions = new Map<string, ImageSetGenerationInfo>();
  versionQueries.forEach((q, i) => {
    const set = withVersions[i];
    if (q.data !== undefined && set !== undefined) versions.set(set.id, q.data);
  });

  const needle = query.trim().toLowerCase();
  const matching = (sets.data ?? []).filter(
    (s) =>
      needle === "" ||
      s.display_name.toLowerCase().includes(needle) ||
      (s.canonical_name ?? "").toLowerCase().includes(needle),
  );
  const mine = new Set([
    me.data?.user_id,
    ...(me.data?.groups.map((g) => g.group_id) ?? []),
  ]);
  const isMine = (s: ImageSetInfo) =>
    !isStandard(s) && s.owner != null && mine.has(s.owner.id);
  const standard = matching.filter(isStandard);
  const yours = matching.filter(isMine);
  const others = matching.filter((s) => !isStandard(s) && !isMine(s));
  const othersShown = showAllOthers ? others : others.slice(0, OTHERS_SHOWN);

  return (
    <>
      <div className="toolbar">
        <h1>Images</h1>
        <span className="spacer" />
        <input
          type="search"
          className="search"
          aria-label="Search images"
          placeholder="Search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <button type="button" onClick={() => setCreating(true)}>
          <Plus size={16} aria-hidden="true" /> New image
        </button>
      </div>
      <NewImageDialog open={creating} onClose={() => setCreating(false)} />

      {sets.isPending && <p className="muted">Loading…</p>}
      <RequestError error={sets.error} />

      {sets.data && (
        <>
          {standard.length > 0 && (
            <section>
              <h2>
                Standard images{" "}
                <HelpTip label="About standard images">
                  Maintained by Treadmill, and built to run on any host.
                </HelpTip>
              </h2>
              <div className="image-cards">
                {standard.map((s) => (
                  <article key={s.id} className="card">
                    <h3>
                      <Link to={`/images/${s.id}`}>{s.display_name}</Link>
                    </h3>
                    {s.canonical_name != null && (
                      <div className="muted mono">{s.canonical_name}</div>
                    )}
                    <div>
                      <Platforms version={versions.get(s.id)} />
                    </div>
                    <div className="image-card-foot">
                      <small className="muted">
                        <Version set={s} version={versions.get(s.id)} />
                      </small>
                      <Link className="btn" to={`/jobs/new?image=${s.id}`}>
                        <Play size={14} aria-hidden="true" /> Run job
                      </Link>
                    </div>
                  </article>
                ))}
              </div>
            </section>
          )}

          <section>
            <h2>Your images</h2>
            {yours.length > 0 ? (
              <ImageRows
                sets={yours}
                versions={versions}
                showOwner={yours.some((s) => s.owner?.id !== me.data?.user_id)}
              />
            ) : (
              <p className="muted">None</p>
            )}
          </section>

          {others.length > 0 && (
            <section>
              <h2>Shared with you &amp; public</h2>
              <ImageRows sets={othersShown} versions={versions} showOwner />
              {others.length > othersShown.length && (
                <button type="button" onClick={() => setShowAllOthers(true)}>
                  Show all {others.length}
                </button>
              )}
            </section>
          )}
        </>
      )}
    </>
  );
}
