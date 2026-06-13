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

footer.site {
  max-width: 920px;
  margin: 2rem auto;
  padding: 0 1.25rem;
  color: var(--muted);
  font-size: 0.85rem;
}
"#;
