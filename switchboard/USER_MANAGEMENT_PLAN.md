# User management & login audit — implementation plan

Status: planned. Two coupled workstreams:

1. **Audit gap-fill** — the OAuth login paths currently produce *no* audit
   events. The audit infrastructure (`src/audit/*`: the `transition`/`emit`
   chokepoint, the `define_event!` macro, the renderer registry, the read API,
   and the `host_permissions`/`job_permissions` engine queries) is fully built
   but **unwired**: `define_event!` is never invoked, no `Transition` is
   implemented, and `persist_event` has only test callers. This workstream
   routes every login write through the chokepoint.
2. **User management API** — self-profile read/update, public profile lookup,
   session management, plus a cleanup of dead API types in `treadmill-rs`.

Builds on `OAUTH_LOGIN_PLAN.md` and `AUDIT_LOG_PLAN.md`.

---

## Cross-cutting decisions (locked in review)

- **Every audit event carries the immutable internal `user_id`** (the
  `subjects.subject_id` UUID), not merely the provider's `provider_user_id`.
  Provider id + login handle are renameable/external; the internal id is the
  durable key the audit trail is keyed and joined on. For login events the actor
  *is* the user, so `actor` already carries it; events are nonetheless written
  with an explicit `user: Subject` field so the immutable id is present in the
  payload independent of who the actor is.
- The profile-refresh-on-login event is a **"change"**, never a "sync":
  `user_profile_changed.v1`.
- CSRF flow bookkeeping (`oauth_flows` insert/consume) stays **unaudited** — the
  one explicitly-exempt login write.
- **Locked accounts are enforced** in this milestone (see §6).

---

## 1. Schema: enrich `api_tokens`

Add to `tml_switchboard.api_tokens` (SCHEMA.sql + a migration regenerated with
`./migrate.sh -c add_token_provenance` in the `.#database` devshell; regenerate
`.sqlx`):

| Column        | Type   | Meaning |
|---------------|--------|---------|
| `user_agent`  | text   | Client UA string at mint (nullable). |
| `comment`     | text   | User-supplied label for a token (nullable). |
| `created_ip`  | text   | Resolved client IP at mint (see §2; nullable). |
| `created_port`| int    | Client port if known from the socket peer (nullable). |

These both feed the sessions-list endpoint (§7) and are the on-token mirror of
the `session_token_issued` audit payload.

## 2. Config + client-address resolution

`ServerConfig` gains:

```rust
/// Header names (priority order) to trust for the real client address when
/// behind a reverse proxy, e.g. ["X-Forwarded-For"]. Empty (default) => trust
/// only the raw socket peer.
#[serde(default)]
pub trusted_proxy_headers: Vec<String>,
```

A helper `client_addr(parts: &Parts, cfg: &ServerConfig) -> ClientAddr`:

- Empty config → use `ConnectInfo<SocketAddr>` (already wired via
  `into_make_service_with_connect_info`): both IP and port.
- Otherwise consult the listed headers in order; first parseable address wins.
  Proxy headers (`X-Forwarded-For`) typically yield IP only → `port = None`.

`ClientAddr { ip: IpAddr, port: Option<u16> }`. Used by the login callback and
token mint. (CIDR-restricted upstream trust is a deferred refinement.)

## 3. Audit visibility extension: self-view

Login/profile events are *about the user themselves*. To let a user see their
own history (`GET /users/{self}/events`) while keeping operator-only rows
admin-restricted:

- Add `ViewPolicy::SelfAccess` (stored as `"self"`).
- Extend `define_event!`'s `Subject @ view(Self)` arm — today it hardcodes
  `OperatorOnly` — to emit a `Subject`-role relation with `SelfAccess`.
- In `audit/feed.rs`, the `subject` branch: when `entity_id == viewer_id`, add
  `"self"` to the viewer's allowed policies. Admins keep global visibility; the
  macro's auto actor relation stays `OperatorOnly` (admin-only).

## 4. Login audit events

Defined with `define_event!`. Granularity: **fine-grained per-mutation, plus a
login marker.** Each carries the immutable `user_id`.

| Event | When | Key payload |
|-------|------|-------------|
| `user_logged_in.v1` | every successful callback | user_id, provider, provider_user_id, login, new_user, client_ip, client_port |
| `user_provisioned.v1` | brand-new account created | user_id, provider, provider_user_id, login, username |
| `oauth_identity_linked.v1` | existing user matched by verified email | user_id, provider, provider_user_id, login |
| `user_profile_changed.v1` | full_name/avatar/provider_login actually changed | user_id, changed fields (old→new) |
| `user_email_added.v1` | a verified email newly inserted | user_id, provider, email |
| `group_membership_changed.v1` | a `github_org` membership added/removed | user_id, group_id, source_ref, added\|removed |
| `session_token_issued.v1` | session token minted | user_id, token_id, expires_at, client_ip, client_port, user_agent |

Notes:
- `user_profile_changed` is **compare-then-write**: emitted only when a field
  differs, so a no-op re-login does not spam the log.
- `user_email_added` uses `INSERT ... RETURNING` so only real inserts emit.
- `group_membership_changed`: one event per added/removed row (restructure the
  reconcile CTE to `RETURNING` the deltas). Relations include the group
  (Context) and the user (Subject @ Self).

## 5. Route login writes through the chokepoint

- `provision_user` / `create_user` / `reconcile_auto_groups` take
  `&mut Transaction` and perform each mutation via `audit::transition(tx, …)`
  with a `Transition` impl whose `apply()` holds the (moved) SQL and returns the
  event. Pure reads (resolve-by-identity, link-by-email, `unique_username`) stay
  plain queries.
- `insert_token` becomes a `SessionTokenIssued` transition carrying the §2
  client address + UA, and writes the new §1 columns.
- The callback emits `user_logged_in` via `audit::emit` once per success.
- Everything runs on the single existing login `tx` → atomic with the commit;
  a rolled-back login leaves no audit rows.

## 6. Locked-account enforcement

- Login callback: after resolving/provisioning, if `users.locked` is true,
  abort before issuing a token (`403`), and emit an audit event
  (`login_denied_locked.v1`, operator-visible).
- `Subject` extractor (`auth/extract.rs`): reject a token whose owning user is
  `locked` (`401/403`), so existing sessions of a since-locked account stop
  working. One extra column on the token lookup (`join users`).

## 7. User-management API

### 7.1 Clean up dead `treadmill-rs` API types

In `treadmill-rs/src/api/switchboard.rs`, remove (zero live users — the only
wired job/host routes are the audit feed + `connect`):

- `JsonProxiedStatus`, `JobResult`, `JobState` (+ its `TryFrom<…> for
  RunningJobState`), `JobEvent`, `ExtendedJobState`, `JobStatus`,
  `JobSshEndpoint`, `HostStatus`.
- The whole `api/switchboard/jobs.rs` and `api/switchboard/hosts.rs` submodules
  (all their `Response`/`Filter` types are dead).
- Prune imports left unused (`RunningJobState`, `JobInitializingStage`,
  `TaskExitStatus`, `StatusCode`).

Keep: `AuthToken`, `LoginResponse`, `WhoAmIResponse`, `JobInitSpec`,
`JobRequest`, `TerminationReason`. Refresh `api-spec/openapi.json` via
`UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard`.

### 7.2 New shared types (`treadmill-rs`)

- `PublicUserProfile { user_id, username, full_name, avatar_url, github:
  Option<LinkedGitHub> }` — the safe, world-readable subset.
- `SelfUserProfile` — public fields **plus** `emails: Vec<String>`, `groups:
  Vec<GroupMembership>` (transitive, via the `principals()` function), `locked`.
  (Emails + memberships are folded into the profile endpoint itself.)
- `GroupMembership { group_id, name, source, source_ref }`.
- `UpdateProfileRequest { full_name, username, avatar_url }` — double-`Option`
  so "absent" and "set null" are distinguishable.
- `SessionInfo { token_id, created_at, expires_at, user_agent, comment,
  created_ip, canceled, current }`.

### 7.3 Endpoints (`src/routes/users.rs`)

| Method/path | Auth | Behavior |
|-------------|------|----------|
| `GET /users/me` | self | `SelfUserProfile` (incl. emails + transitive groups). |
| `PATCH /users/me` | self | Update full_name / username / avatar_url. |
| `GET /users/{id}` | any authed | `PublicUserProfile` (safe subset only). |
| `GET /users/me/tokens` | self | `Vec<SessionInfo>`, flags the current token. |
| `DELETE /users/me/tokens/{token_id}` | self | Revoke (cancel) own session. |
| `GET /users/{id}/events` | self/admin | Audit feed; relaxed for self (§3). |

Validation on `PATCH`:
- **username**: `^[a-z0-9][a-z0-9-]{1,38}$`, reserved-name blocklist
  (`me`, `admin`, `system`, `new`, …), DB-unique → clean **409** on collision
  (not a 500).
- **avatar_url**: parse as `url::Url`, require `http`/`https`, bounded length.

Mutations routed through the chokepoint:
- `user_renamed.v1` (username; carries user_id, old→new).
- `user_profile_updated.v1` (full_name/avatar; carries user_id, changed fields).
- `session_token_revoked.v1` (carries user_id, token_id).

## 8. Tests

- Extend `tests/oauth_login.rs`: assert the expected `audit_events` +
  relations exist after login, each carrying the immutable `user_id`,
  `provider="github"`, `provider_user_id="12345"`, and a populated `client_ip`.
- New `tests/user_routes.rs`: `GET/PATCH /users/me` (rename uniqueness 409 +
  validation, emails/groups folding), public-subset `GET /users/{id}`, sessions
  list + revoke, self events feed, and a locked-account rejection.
- Refresh `api-spec/openapi.json`.

## 9. Commit sequence (fine-grained)

1. This plan.
2. Schema: `api_tokens` columns + migration + `.sqlx`.
3. Config: trusted-proxy headers + `client_addr` helper.
4. Audit visibility: `SelfAccess` + macro + feed self-view.
5. Login events + chokepoint wiring + addr/UA capture.
6. Locked-account enforcement.
7. `treadmill-rs` cleanup + new user API types + OpenAPI refresh.
8. User-management routes + their audit events + validation.
9. Tests + snapshot refresh.

Build/verify requires the `.#database` devshell (migration regen + `sqlx`
compile-time query checks); run the suite with `cargo nextest`.
