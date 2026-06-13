# Treadmill Console — prototype plan

> Scratch planning doc for the first `treadmill-console` web-frontend prototype.
> Safe to delete once the work lands. Authoritative dev workflow is `AGENTS.md`.

## Goal

A web frontend ("console") for Treadmill's switchboard. First prototype target:
**GitHub OAuth login** (browser, no JS) + a **user profile page** (`/me`),
both working against the current switchboard.

## Hard requirements (from the user)

- **Type-checked end to end.** A breaking API change must surface as a Rust
  *compile error* in the frontend, with no codegen step and no spec drift.
- **Stability / low maintenance** over features. Free-time project.
- **Rust** (the rest of the project is Rust).
- **No reliance on JS/WASM** in the browser — server-side rendering, optional
  progressive enhancement later.
- **Frontend not tightly coupled to the API server** — always separable, even if
  initially co-hosted. Sharing a *types crate* (compile-time) is fine; sharing a
  router/process (runtime) is not.

## Chosen architecture

- **Standalone Rust SSR binary**, its own crate. Talks to switchboard purely
  over HTTP — no shared router, separately deployable.
- **Type safety via the shared `treadmill-rs` crate**, not OpenAPI codegen. The
  console deserializes responses into the *same* structs switchboard serializes
  from, so a field change recompiles both sides. Zero drift, zero codegen.
- **A typed client in `treadmill-rs`** (one fn per endpoint, returns the shared
  types), behind a `client` cargo feature so non-HTTP consumers don't pull
  `reqwest`. Shared by the console *and* an eventual CLI. Closes the
  route/method gap (a renamed endpoint becomes a compile error too), not just
  the payload gap.
- **axum + Maud** for SSR. Maud = compile-checked HTML macros (nothing to
  drift). **No htmx yet** — pure full-page SSR; add htmx per-element later.
- **Classless CSS** (`simple.css`, pinned + embedded) so HTML stays semantic
  (no Tailwind/utility classes). Light-gray background, sr.ht / simple-GitHub
  feel.

### Rejected alternatives

- **Elm** — language is stable and pleasant, but it's a JS SPA (fails no-JS) and
  its type-safety runs through OpenAPI codegen (drift risk, extra toolchain).
- **Leptos/Dioxus/Sycamore** — WASM-centric (fails no-JS), fast-churn (fails
  low-maintenance), server-function model tends to couple FE/BE (fails
  separability).

## Key design decisions

### Login bridge (the hard part)

Switchboard's OAuth callback returns the session token as a **JSON body** (built
for CLI/programmatic clients). A no-JS browser can't capture that. So browser
login *requires a small switchboard change*.

**Chosen (prototype):** add an optional `oauth.github.browser_success_redirect`
config to switchboard. When set, `github_callback` `302`-redirects the browser
to that URL with `?token=…&expires_at=…` instead of returning JSON (JSON path
preserved when unset). The console's `/auth/landing` stores the token in its
cookie and immediately redirects to `/` to strip it from the address bar.

**Deferred hardening** (in `TODOS.md`): per-request `return_to` allowlist +
back-channel one-time-code exchange, so the token never transits a URL.

### Session / cookie

- **Stateless console.** The cookie holds the **raw switchboard bearer token**,
  `HttpOnly` + `Secure` + `SameSite=Lax`, `Max-Age` from `expires_at`.
- **No signing/encryption.** The cookie carries only a bearer credential that
  switchboard re-validates every request; the client can't forge a valid one and
  it's the user's own token. (Signing would only matter if the cookie held
  server-trusted *claims*, which it won't.)
- `Secure` flag follows whether `public_base_url` is https (so localhost
  plain-HTTP dev still works).
- A `401` from switchboard → clear cookie, redirect to `/login`.

### `/me` page scope

Profile + sessions + audit:
- **Profile** (`GET /users/me`): avatar (direct `<img>` to GitHub URL), full
  name, `@username`, linked GitHub, verified emails, group memberships, locked.
- **Sessions** (`GET /users/me/tokens`): table; current session flagged.
- **Audit feed** (`GET /users/{id}/events`): rendered via a **reusable
  `audit_feed` component**, most-recent-first head of the log.

**Convention:** every resource page shows the head of that resource's reversed
audit log at the bottom, via the same component.

## Required shared-type change

`AuditFeedResponse` + `RenderedAuditRow` currently live in
`switchboard/src/audit/feed.rs` and derive only `Serialize, JsonSchema` (no
`Deserialize`, not shared). They must **move into `treadmill-rs`
(`api/switchboard/audit.rs`)** and gain `Deserialize`; switchboard imports them
back (same pattern as the `users`/`images` API types). This trips the OpenAPI
snapshot guard → regenerate deliberately.

## Crate / config

- Folder `console/`, package **`treadmill-console`** (mirrors
  `treadmill-switchboard`). Added to workspace `members`.
- Console config: `bind_address`, `public_base_url`, `switchboard_base_url`.
- No DB, no SQL, no migrations in the console.

## Build / verify (per AGENTS.md — all via Nix)

- `nix develop --command bash -c 'cargo build -p <crate>'` / `cargo clippy
  --all-targets -- -D warnings` (must stay warning-free).
- `nix fmt -- <changed files>` (scope it; switchboard has pre-existing drift).
- Switchboard: `nextest-db` + `switchboard-migrations-consistency` checks; OpenAPI
  snapshot regen: `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard --test
  openapi_spec`.

## Implementation phases (one commit each)

0. **TODOS.md** — record the login-hardening deferral. *(separate commit)*
0b. **This plan file.** *(separate commit)*
1. **Move audit types** to `treadmill-rs` + switchboard re-export + OpenAPI regen.
2. **`client` feature + `SwitchboardClient`** in `treadmill-rs`.
3. **`browser_success_redirect`** in switchboard (config + callback).
4. **Scaffold `treadmill-console`**: workspace member, config, cookie session,
   `/login`, `/auth/landing`, `/logout`, `/static` (embedded CSS).
5. **`/me` page** + reusable `audit_feed` component + simple.css styling.
