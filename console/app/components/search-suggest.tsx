import { useState, type ReactNode } from "react";

const SHOWN = 6;

function fuzzyMatches(query: string, text: string): boolean {
  let matched = 0;
  for (const c of text.toLowerCase()) {
    if (c === query[matched]) matched++;
    if (matched === query.length) return true;
  }
  return query.length === 0;
}

function rank<T>(options: T[], query: string, words: (o: T) => string[]) {
  const q = query.trim().toLowerCase();
  const exact = options.filter((o) =>
    words(o).some((w) => w.toLowerCase().includes(q)),
  );
  const fuzzy = options.filter(
    (o) => !exact.includes(o) && words(o).some((w) => fuzzyMatches(q, w)),
  );
  return [...exact, ...fuzzy];
}

export function SearchSuggest<T>({
  id,
  placeholder,
  options,
  keyOf,
  words,
  render,
  onPick,
  autoFocus,
}: {
  id: string;
  placeholder: string;
  options: T[];
  keyOf: (option: T) => string;
  words: (option: T) => string[];
  render: (option: T) => ReactNode;
  onPick: (option: T) => void;
  autoFocus?: boolean;
}) {
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const matches = rank(options, query, words);
  const shown = matches.slice(0, SHOWN);

  function pick(option: T) {
    setQuery("");
    setOpen(false);
    onPick(option);
  }

  return (
    <div
      className="suggest"
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget)) setOpen(false);
      }}
    >
      <input
        id={id}
        type="search"
        autoComplete="off"
        autoFocus={autoFocus}
        placeholder={placeholder}
        value={query}
        onFocus={() => setOpen(true)}
        onChange={(e) => {
          setQuery(e.target.value);
          setOpen(true);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            if (shown[0] !== undefined) pick(shown[0]);
          }
          if (e.key === "Escape") setOpen(false);
        }}
      />
      {open && (
        <ul className="suggest-list">
          {shown.map((option) => (
            <li key={keyOf(option)}>
              <button type="button" onClick={() => pick(option)}>
                {render(option)}
              </button>
            </li>
          ))}
          {matches.length > SHOWN && (
            <li className="muted">… {matches.length - SHOWN} more</li>
          )}
          {matches.length === 0 && <li className="muted">No matches</li>}
        </ul>
      )}
    </div>
  );
}
