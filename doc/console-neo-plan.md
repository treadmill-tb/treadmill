# SPA Web Console (`console-neo/`) — Roadmap

Status of the SPA console bootstrap and the remaining build-out. The
architecture decisions and their rationale live in the bootstrap commit
messages; the short version: React 19 + React Router v7 in framework mode with
`ssr: false` (a static SPA today; the progressive-enhancement path later is
`ssr: true` plus per-route loaders/actions on a serverless runtime), an API
client generated from `switchboard/api-spec/openapi.yaml`
(openapi-typescript → committed `app/api/schema.d.ts`, drift-checked by the
`console-neo` Nix package), openapi-fetch + openapi-react-query for typed
calls, TanStack Query for caching. See AGENTS.md §2 for the dev loop.

## Done

- **Phase 0 — scaffold + Nix wiring.** Toolchain, generated API types with
  drift gate, lint/typecheck/build as a flake check, prettier via treefmt.
- **Phase 1 — auth.** `/login` from `/auth/providers` (OAuth + dev-only mock
  identities), `/login/callback` staged-login completion incl. the ToS
  interstitial (409 retry with the fresh pair), bearer token in localStorage,
  401 → logout middleware.
- **Phase 2 — read views.** Lists + details + audit feeds for jobs, hosts,
  images, image groups (generations, grants), users, own profile/tokens.
  Reusable pieces: `AuditLog` (one component, all four event feeds),
  exhaustive-switch badges over generated unions, entity links, relative
  times, tag chips, digests.

Mutation affordances exist but render disabled, marked `TODO(console-neo)` at
each site.

## Remaining phases (hand-off prompts)

Each block below is a self-contained prompt for a follow-up agent.

### Phase 3 — mutations

> In `console-neo/`, implement the write surface against the generated `$api`
> client (`app/api/client.ts`): enqueue-job form (`POST /jobs` — JobInitSpec
> variants image/image_group/resume/restart, ssh_keys, host/target tag
> requirements, parameters incl. the `secret` flag, restart policy, owner
> picker from the `/users/me` groups), terminate job, register image, create
> image group / generation / grant, revoke grant, set group public/private,
> profile PATCH, token revoke. Replace the disabled affordances marked
> `TODO(console-neo)`. Use TanStack Query mutations ($api.useMutation) with
> invalidation of the affected query keys; no optimistic updates. Confirmation
> dialog for destructive actions (terminate, revoke). Keep runtime checks out:
> derive all types from `app/api/schema.d.ts`.

### Phase 4 — job console log viewer

> Implement doc/log-streaming-plan.md §4b in `console-neo/`: on the job detail
> page, replace the "Console log" placeholder with a live viewer. `POST
> /jobs/{id}/nats-log-token`, connect `nats.ws` to the returned `nats_url`
> with the bearer JWT, subscribe the returned `subject` wildcard, and pipe
> payloads into `@xterm/xterm`. Re-request credentials when reconnecting after
> roughly `expires_in_secs` (an established connection survives JWT expiry).
> A 503 from the token route means log streaming is disabled on the
> deployment: hide the panel. Note the NATS server's websocket listener
> validates `Origin` (`allowed_origins`) — the console origin must be
> configured there.

### Phase 5 — legacy console removal

> Remove the `console/` crate: drop it from the Cargo workspace and from
> `switchboard` (the `treadmill-console` dependency, the `[console]`
> embedded-console config section, `embedded_console_router`, and the
> embedded-console implicit `return_to` allowance in
> `switchboard/src/config.rs`), keeping the reqwest helpers in `treadmill-rs`
> for the future CLI. Then `git mv console-neo console` and adjust
> `nix/console.nix` (paths, package name), `nix/treefmt.nix` excludes/includes,
> AGENTS.md, and this document.

## Deployment notes

The SPA is static and hosted separately (e.g. Cloudflare/GitHub Pages) with
`VITE_TML_API_URL` set to the switchboard origin at build time. The
switchboard needs the console origin in `server.cors_allowed_origins`, and the
exact URL `<console-origin>/login/callback` in `oauth.return_to_allowlist`.
The host serving the SPA must rewrite unknown paths to `index.html` (SPA
fallback).
