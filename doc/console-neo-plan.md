# SPA Web Console (`console/`) — Roadmap

Status of the SPA console bootstrap and the remaining build-out. The
architecture decisions and their rationale live in the bootstrap commit
messages; the short version: React 19 + React Router v7 in framework mode with
`ssr: false` (a static SPA today; the progressive-enhancement path later is
`ssr: true` plus per-route loaders/actions on a serverless runtime), an API
client generated from `switchboard/api-spec/openapi.yaml`
(openapi-typescript → committed `app/api/schema.d.ts`, drift-checked by the
`console` Nix package), openapi-fetch + openapi-react-query for typed
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
- **Phase 3 — mutations.** The full write surface via `$api.useMutation` with
  query-key invalidation: enqueue-job form at `/jobs/new` (all JobInitSpec
  variants, ssh keys, host/target tag requirements, parameters incl. secret,
  restart policy, owner picker), terminate job, register image, create image
  group / generation / grant, revoke grant, set group public/private, profile
  PATCH, token revoke. Destructive actions confirm via `window.confirm`.

- **Phase 4 — job console log viewer.** `JobLog`
  (`app/components/job-log.tsx`) on the job detail page: `POST
  /jobs/{id}/nats-log-token`, `@nats-io/nats-core` `wsconnect` with the bearer
  JWT
  (`reconnect: false` — the component's own loop re-requests credentials on
  every reconnect), output piped into `@xterm/xterm` (+fit addon). A 503
  (streaming disabled) hides the panel. **Replay-then-follow:** the read
  token grants, beyond subscribe on `logs.<id>.>`, the job-scoped slice of
  the JetStream API (consumer create/info/next/delete plus stream info on
  `logs-<id>`, replies restricted to inboxes under the per-job
  `inbox_prefix`), so `JobLog` runs a single `@nats-io/jetstream` ordered
  consumer: it replays a bounded slice of stored history (default in
  `job-log.tsx`; `?replay=` overrides, `?replay=0` = live-only) from a start
  sequence estimated off `STREAM.INFO` byte/message counts, then keeps
  following live, and resumes after the last seen sequence on reconnect.
  Truncated replay starts on a message boundary and skips to the first
  newline — deliberately *not* escape-sequence-safe; xterm resyncs. The
  permission set is proven sufficient by the `nats_live_read_token_scope_…`
  test in the `nats-log-streaming` Nix check.

- **Phase 5 — legacy console removal.** Dropped the server-side-rendered
  `console/` crate: out of the Cargo workspace and off `switchboard` (the
  `treadmill-console` dependency, the `[console]` embedded-console config
  section + `EmbeddedConsoleConfig`, `embedded_console_router`, and the
  implicit `return_to` allowance for its landing URL), keeping the
  `treadmill-rs` `client` reqwest helpers for the future CLI. Renamed
  `console-neo/` → `console/` and repointed `nix/console.nix`,
  `nix/treefmt.nix`, `nix/devshells.nix`, AGENTS.md, and this document. The
  dev stack (`tools/devstack.sh`, wrapped by `nix/apps.nix`) now serves the
  SPA as a **separate-origin static site** (`static-web-server` with
  history-API fallback), rebuilt with `VITE_TML_API_URL` pinned to the
  devstack switchboard and the switchboard's `cors_allowed_origins` +
  `oauth.return_to_allowlist` pointed back at the console origin.

## Deployment notes

The SPA is static and hosted separately (e.g. Cloudflare/GitHub Pages) with
`VITE_TML_API_URL` set to the switchboard origin at build time. The
switchboard needs the console origin in `server.cors_allowed_origins`, and the
exact URL `<console-origin>/login/callback` in `oauth.return_to_allowlist`.
For the log viewer, `log_streaming.websocket_url` must point at the NATS
server's `websocket` listener (`wss://` in production, with the console origin
in the listener's `allowed_origins`); browsers cannot use the plain
`nats_url`.
The host serving the SPA must rewrite unknown paths to `index.html` (SPA
fallback).
