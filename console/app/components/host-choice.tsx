import { useRef, useState } from "react";

import { HostItem, HostLine, type HostCandidate } from "./job-preview";
import { SearchSuggest } from "./search-suggest";

export type HostMode = "any" | "single" | "filter";

const EXAMPLES = [
  'host.site == "site"',
  'host.duts.exists(d, d.board == "board")',
  "has(host.labels.key)",
  " && ",
];

function FilterEditor({
  value,
  onChange,
}: {
  value: string;
  onChange: (value: string) => void;
}) {
  const textarea = useRef<HTMLTextAreaElement>(null);

  function insert(example: string) {
    const el = textarea.current;
    const start = el?.selectionStart ?? value.length;
    const end = el?.selectionEnd ?? value.length;
    onChange(value.slice(0, start) + example + value.slice(end));
    requestAnimationFrame(() => {
      el?.focus();
      el?.setSelectionRange(start + example.length, start + example.length);
    });
  }

  return (
    <>
      <textarea
        ref={textarea}
        className="mono"
        rows={2}
        spellCheck={false}
        aria-label="Filter expression"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      <div className="examples">
        <small>Examples · insert at cursor</small>
        {EXAMPLES.map((example) => (
          <button
            key={example}
            type="button"
            className="mono"
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => insert(example)}
          >
            {example.trim() === "&&" ? "&& (and)" : example}
          </button>
        ))}
      </div>
    </>
  );
}

function SingleHost({
  hostId,
  onHostId,
  candidates,
}: {
  hostId: string | null;
  onHostId: (hostId: string) => void;
  candidates: HostCandidate[];
}) {
  const [searching, setSearching] = useState(false);
  const selected = candidates.find((c) => c.match.host_id === hostId);

  if (selected !== undefined && !searching) {
    return (
      <div className="host-items">
        <HostItem
          candidate={selected}
          action={
            <button
              type="button"
              className="link-btn"
              onClick={() => setSearching(true)}
            >
              Change
            </button>
          }
        />
      </div>
    );
  }
  return (
    <SearchSuggest
      id="single-host"
      placeholder="Search hosts"
      autoFocus
      options={candidates}
      keyOf={(c) => c.match.host_id}
      words={(c) => [
        c.match.name,
        c.host?.spec?.site ?? "",
        ...(c.host?.spec?.duts.map((d) => d.board) ?? []),
      ]}
      render={(c) => <HostLine candidate={c} />}
      onPick={(c) => {
        setSearching(false);
        onHostId(c.match.host_id);
      }}
    />
  );
}

export function HostChoice({
  mode,
  onMode,
  hostId,
  onHostId,
  filter,
  onFilter,
  candidates,
}: {
  mode: HostMode;
  onMode: (mode: HostMode) => void;
  hostId: string | null;
  onHostId: (hostId: string) => void;
  filter: string;
  onFilter: (filter: string) => void;
  candidates: HostCandidate[];
}) {
  const option = (value: HostMode) => (
    <input
      id={`host-mode-${value}`}
      type="radio"
      name="host-mode"
      checked={mode === value}
      onChange={() => onMode(value)}
    />
  );

  return (
    <fieldset className="options">
      <legend>Host</legend>
      <label>
        {option("any")}
        <span className="option-body">
          <strong>Any host</strong>
        </span>
      </label>
      <div>
        {option("single")}
        <span className="option-body">
          <label htmlFor="host-mode-single">
            <strong>Single host</strong>
          </label>
          {mode === "single" && (
            <SingleHost
              hostId={hostId}
              onHostId={onHostId}
              candidates={candidates}
            />
          )}
        </span>
      </div>
      <div>
        {option("filter")}
        <span className="option-body">
          <label htmlFor="host-mode-filter">
            <strong>Filter expression</strong>
          </label>
          {mode === "filter" && (
            <FilterEditor value={filter} onChange={onFilter} />
          )}
        </span>
      </div>
    </fieldset>
  );
}
