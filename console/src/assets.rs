//! Static assets served by the console.
//!
//! The stylesheet is embedded as a string constant rather than read from disk:
//! it keeps the binary self-contained (no runtime asset path to configure) and
//! keeps the file inside the crate's Rust sources, so the Nix build needs no
//! extra fileset wiring to ship it.

/// `text/css` for the console. A small, classless sheet: it styles plain
/// semantic HTML (no utility classes), with a light-gray page, a centered
/// column, and restrained typography in the spirit of sr.ht / a plain GitHub.
pub const STYLE_CSS: &str = r#"
:root {
  --bg: #f4f5f6;
  --surface: #ffffff;
  --border: #d8dadd;
  --text: #1f2328;
  --muted: #6b7280;
  --link: #0969da;
  --accent: #0969da;
  --radius: 6px;
}

* { box-sizing: border-box; }

html { font-size: 16px; }

body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica,
    Arial, sans-serif;
  line-height: 1.5;
}

a { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }

header.site {
  background: var(--surface);
  border-bottom: 1px solid var(--border);
}

header.site nav {
  max-width: 920px;
  margin: 0 auto;
  padding: 0.75rem 1.25rem;
  display: flex;
  align-items: baseline;
  gap: 1.25rem;
}

header.site nav .brand {
  font-weight: 600;
  color: var(--text);
}

header.site nav .spacer { flex: 1; }

main {
  max-width: 920px;
  margin: 1.5rem auto;
  padding: 0 1.25rem;
}

h1 { font-size: 1.5rem; margin: 0 0 1rem; }
h2 {
  font-size: 1.1rem;
  margin: 1.75rem 0 0.75rem;
  padding-bottom: 0.35rem;
  border-bottom: 1px solid var(--border);
}

section.card {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 1.1rem 1.25rem;
  margin: 0 0 1.25rem;
}

dl.fields {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 0.4rem 1rem;
  margin: 0;
}
dl.fields dt { color: var(--muted); }
dl.fields dd { margin: 0; }

table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.95rem;
}
th, td {
  text-align: left;
  padding: 0.45rem 0.6rem;
  border-bottom: 1px solid var(--border);
  vertical-align: top;
}
th { color: var(--muted); font-weight: 600; }
tbody tr:last-child td { border-bottom: none; }

code, time { font-variant-numeric: tabular-nums; }
time { color: var(--muted); }

.tag {
  display: inline-block;
  padding: 0 0.4rem;
  border: 1px solid var(--border);
  border-radius: 999px;
  font-size: 0.8rem;
  color: var(--muted);
}
.tag.current {
  color: var(--accent);
  border-color: var(--accent);
}

.muted { color: var(--muted); }
.empty { color: var(--muted); font-style: italic; }

/* A heading row with a trailing action button (e.g. job pages). */
.page-head {
  display: flex;
  align-items: center;
  gap: 1rem;
  margin-bottom: 1rem;
}
.page-head h1 { margin: 0; flex: 1; }

/* Inline action <button> in a form (e.g. Terminate); anchors use a.button. */
button.button {
  display: inline-block;
  padding: 0.4rem 0.8rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font: inherit;
  font-weight: 600;
  cursor: pointer;
}
button.button:hover { border-color: var(--accent); color: var(--accent); }
.button.danger { color: #b3261e; border-color: #e5b3b0; }
.button.danger:hover { color: #fff; background: #b3261e; border-color: #b3261e; }

/* Job-state badges: tint the shared .tag pill per lifecycle phase. */
.tag.state-pending { color: var(--muted); }
.tag.state-busy { color: #8a6d00; border-color: #d4a72c; }
.tag.state-ready { color: #1a7f37; border-color: #4ac26b; }
.tag.state-done { color: var(--text); }

/* Placeholder panel for the not-yet-wired live console. */
section.card.console-placeholder {
  background: #0d1117;
  border-color: #0d1117;
  min-height: 8rem;
  display: flex;
  align-items: center;
  justify-content: center;
}
section.card.console-placeholder .muted { color: #8b949e; }

/* Dispatch form. */
.form-error {
  border: 1px solid #e5b3b0;
  background: #fdecea;
  color: #b3261e;
  border-radius: var(--radius);
  padding: 0.6rem 0.9rem;
  margin: 0 0 1rem;
}
.form-field { margin: 0 0 0.9rem; }
.form-field label {
  display: block;
  color: var(--muted);
  font-weight: 600;
  margin-bottom: 0.25rem;
}
.form-field input,
.form-field select { min-width: 18rem; max-width: 100%; }
input[type="text"], input[type="number"], select {
  padding: 0.35rem 0.5rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font: inherit;
}
.row {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  margin-bottom: 0.4rem;
  flex-wrap: wrap;
}
.row input[type="text"] { flex: 1; min-width: 12rem; }
.secret-toggle { color: var(--muted); white-space: nowrap; }
.button.row-add, .button.row-remove {
  padding: 0.2rem 0.6rem;
  font-size: 0.85rem;
  font-weight: 500;
}
.target {
  border: 1px dashed var(--border);
  border-radius: var(--radius);
  padding: 0.75rem;
  margin-bottom: 0.75rem;
}
.target-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 0.5rem;
}
.form-actions {
  display: flex;
  align-items: center;
  gap: 1rem;
  margin-top: 1rem;
}

/* Login page: a vertical stack of provider buttons. */
.login-options {
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
  margin-top: 0.5rem;
}
a.button {
  display: block;
  padding: 0.55rem 0.9rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-weight: 600;
  text-align: center;
}
a.button:hover { border-color: var(--accent); color: var(--accent); text-decoration: none; }

/* The development-only mock sign-in block, visually set apart and warned. */
section.card.dev { border-color: #d4a72c; background: #fff8e6; }
section.card.dev h2 { margin-top: 0; }
.warning { color: #8a6d00; }

footer.site {
  max-width: 920px;
  margin: 2rem auto;
  padding: 0 1.25rem;
  color: var(--muted);
  font-size: 0.85rem;
}
"#;
