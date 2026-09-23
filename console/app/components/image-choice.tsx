import { useState } from "react";

import { isStandard } from "../api/images";
import type { components } from "../api/schema";
import { SearchSuggest } from "./search-suggest";

type ImageSetInfo = components["schemas"]["ImageSetInfo"];
type HostListEntry = components["schemas"]["HostListEntry"];

function fitsHost(set: ImageSetInfo, host: HostListEntry | undefined): boolean {
  if (host === undefined) return true;
  const profiles = host.spec?.platform.profiles ?? [];
  return set.platforms.some((p) => profiles.includes(p));
}

function Platforms({
  set,
  host,
}: {
  set: ImageSetInfo;
  host: HostListEntry | undefined;
}) {
  if (!fitsHost(set, host)) {
    return (
      <small className="incompatible">not compatible with this host</small>
    );
  }
  return (
    <span>
      {set.platforms.map((p) => (
        <span key={p} className="chip mono">
          {p}
        </span>
      ))}
    </span>
  );
}

function versionsOf(set: ImageSetInfo): number[] {
  const latest = set.latest_generation ?? 0;
  return Array.from({ length: latest - 1 }, (_, i) => latest - 1 - i);
}

export function ImageChoice({
  sets,
  value,
  onChange,
  version,
  onVersion,
  host,
}: {
  sets: ImageSetInfo[];
  value: string | null;
  onChange: (setId: string) => void;
  version: number | null;
  onVersion: (version: number | null) => void;
  host: HostListEntry | undefined;
}) {
  const [searching, setSearching] = useState(false);
  const runnable = sets.filter((s) => s.latest_generation != null);
  const standard = runnable.filter(isStandard);
  const others = runnable.filter((s) => !isStandard(s));
  const selected = runnable.find((s) => s.id === value);
  const otherSelected = selected !== undefined && !isStandard(selected);

  function pick(setId: string) {
    setSearching(false);
    onVersion(null);
    onChange(setId);
  }

  return (
    <div className="form-section">
      <fieldset className="options">
        <legend>Image</legend>
        {standard.map((s) => (
          <label key={s.id} className={fitsHost(s, host) ? undefined : "dim"}>
            <input
              type="radio"
              name="image"
              checked={value === s.id}
              onChange={() => pick(s.id)}
            />
            <span className="option-body">
              <span className="option-title">
                <strong>{s.display_name}</strong>
                {s.canonical_name != null && (
                  <span className="muted mono">{s.canonical_name}</span>
                )}
              </span>
              <Platforms set={s} host={host} />
            </span>
            <small className="muted">v{s.latest_generation}</small>
          </label>
        ))}
        <div
          className={otherSelected && !fitsHost(selected, host) ? "dim" : ""}
        >
          <input
            type="radio"
            name="image"
            aria-label="Other image"
            checked={otherSelected}
            disabled={!otherSelected}
            readOnly
          />
          {otherSelected && !searching ? (
            <>
              <span className="option-body">
                <span className="option-title">
                  <strong>{selected.display_name}</strong>
                  {selected.canonical_name != null && (
                    <span className="muted mono">
                      {selected.canonical_name}
                    </span>
                  )}
                </span>
                <Platforms set={selected} host={host} />
              </span>
              <span className="option-aside">
                <small className="muted">v{selected.latest_generation}</small>
                <button
                  type="button"
                  className="link-btn"
                  onClick={() => setSearching(true)}
                >
                  Change
                </button>
              </span>
            </>
          ) : (
            <span className="option-body">
              <SearchSuggest
                id="other-image"
                placeholder="Other image: name or ID"
                autoFocus={searching}
                options={others}
                keyOf={(s) => s.id}
                words={(s) => [s.display_name, s.canonical_name ?? "", s.id]}
                render={(s) => (
                  <>
                    <strong>{s.display_name}</strong>{" "}
                    <small className="muted">
                      v{s.latest_generation} · {s.platforms.join(", ")}
                    </small>
                  </>
                )}
                onPick={(s) => pick(s.id)}
              />
            </span>
          )}
        </div>
      </fieldset>
      {selected !== undefined && (
        <label className="inline-field">
          Version
          <select
            value={version ?? ""}
            onChange={(e) =>
              onVersion(e.target.value === "" ? null : Number(e.target.value))
            }
          >
            <option value="">latest (v{selected.latest_generation})</option>
            {versionsOf(selected).map((n) => (
              <option key={n} value={n}>
                v{n}
              </option>
            ))}
          </select>
        </label>
      )}
    </div>
  );
}
