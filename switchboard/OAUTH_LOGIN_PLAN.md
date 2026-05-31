# OAuth login + identity provisioning — implementation plan

Status: planned. Scope: switchboard server-side GitHub OAuth login, user
provisioning, verified-email linking, GitHub-org auto-group reconciliation, and
session-token issuance. Builds on the identity/group/authz schema introduced in
`SCHEMA.sql` (commit "switchboard: redesign identity, groups, and authorization
schema").

CLI integration and e2e/cli CI are explicitly **out of scope** for this
milestone (those workflows are being removed). The full grant-based
authorization rewrite is **deferred** to a follow-up milestone; this milestone
ships a deliberately permissive authorization shim (see §7).

---

## 1. Goals and non-goals

In scope:
- `oauth2`-crate GitHub authorization-code flow (no PKCE; GitHub OAuth apps use
  `client_secret`).
- Auto-provision a `users` row (+ `subjects`, `user_identities`, `user_emails`)
  on first login; resolve an existing user on subsequent logins.
- Link a login to an existing user by matching **verified** email.
- Reconcile `github_org` auto-group memberships on every login (lazy path).
- Issue a full-authority `api_tokens` session token.
- A local, third-party-free integration test (wiremock + `#[sqlx::test]`).

Out of scope (this milestone):
- CLI login (loopback/device flow) and its CI.
- Per-token scoping (tokens act with full subject authority).
- The real grant-based permission checks (shimmed; see §7).
- Periodic/background auto-group sweep (only the lazy-on-login path lands now).
- Avatar blob caching (store the URL only).

---

## 2. Schema changes

### 2.1 New table: `oauth_flows`

Short-lived CSRF/flow state, DB-backed so it survives restarts and works across
multiple switchboard instances (the axum `oauth2` example keeps this in an
in-memory cookie session; we deliberately do not).

```sql
create table tml_switchboard.oauth_flows
(
    state      text                     not null primary key, -- CsrfToken secret
    provider   text                     not null,
    created_at timestamp with time zone not null default current_timestamp,
    expires_at timestamp with time zone not null
);
```

The callback consumes (deletes) the row and rejects unknown/expired states.

### 2.2 Refinement to `group_members` (correctness fix)

The committed `group_members` PK is `(group_id, member_id, source)`. That cannot
represent a user who belongs to one Treadmill group via **two** different GitHub
orgs: leaving one org should not drop the membership the other still justifies.
To model multi-justification correctly, `source_ref` must be part of the key.

Change:
- `source_ref` becomes `not null default ''` (manual memberships use `''`).
- PK becomes `(group_id, member_id, source, source_ref)`.

Membership is then "exists any row"; reconciliation is per-`(group, source_ref)`
and never touches `source = 'manual'` rows. The acyclicity trigger is unchanged
(it ignores `source`/`source_ref`).

### 2.3 Migration + snapshot regeneration

- Regenerate migrations from `SCHEMA.sql` via `./migrate.sh -c add_oauth_login`
  inside the `.#database` devshell; verify with `./migrate.sh -v`.
- Refresh the OpenAPI snapshot: `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard`.
- Rewrite `FIXTURES.sql` for the new schema (seed the admin group is already in
  `SCHEMA.sql`; fixtures should seed a sample user/subject/token and a supervisor
  owned by a subject).

---

## 3. Code to delete / rewrite to compile against the new schema

These currently reference dropped columns/tables and will fail `sqlx`'s
compile-time checks:

| File | Action |
|------|--------|
| `src/routes/tokens.rs` | Delete the password `login` handler entirely (argon2, `users.name/email/password_hash`, `LoginRequest`). File likely removed; route dropped from `routes/mod.rs`. |
| `src/sql/api_token.rs` | Drop `inherits_user_permissions` from `SqlApiTokenMetadata` and both `fetch_metadata_*` queries; rewrite `insert_token` for the new 6-column `api_tokens` shape (no perms flag). Remove `TokenPerms`. |
| `src/sql/perm.rs` | Delete `sql_add_user_privileges` (table gone). File removed. |
| `src/auth/db.rs` | Replace `DbPermSearcher`'s `user_privileges`/`api_token_privileges` queries with the permissive shim (§7). |
| `src/perms.rs` | Keep the `PrivilegedAction` action types and route plumbing; their string keys become inert under the shim. No schema references here. |
| `treadmill-rs/src/api/switchboard.rs` | Remove `LoginRequest`; keep `LoginResponse { token, expires_at }`. Add a `WhoAmI`/`Me` response type. |
| Cargo deps | Remove `argon2`. Add `oauth2` (v5, reqwest backend), `reqwest`, `url`; add `wiremock` as a dev-dependency. |

`src/auth/mod.rs`'s `Subject`/`SubjectDetail` stay; `SubjectDetail` keeps
`token_id`/`user_id` (now a subject id).

---

## 4. Configuration

```toml
[oauth.github]
client_id     = "..."                  # secret via TML_OAUTH__GITHUB__CLIENT_ID
client_secret = "..."                  # secret via TML_OAUTH__GITHUB__CLIENT_SECRET
redirect_url  = "https://sb.example/api/v1/auth/github/callback"
# Overridable so tests/dev can point at a local fake; default to github.com:
auth_url     = "https://github.com/login/oauth/authorize"
token_url    = "https://github.com/login/oauth/access_token"
api_base_url = "https://api.github.com"
scopes       = ["read:user", "user:email", "read:org"]
```

`config.rs`: add `oauth: OAuthConfig { github: Option<GithubOAuthConfig> }`. The
overridable `token_url` / `api_base_url` are the entire basis of local
testability (§8). `client_secret` is loaded from env via the existing figment
overlay.

---

## 5. OAuth flow (server)

Routes (under `/api/v1`), implemented in a new `src/routes/auth.rs`:

- `GET /auth/github/login`
  1. Build the `oauth2` client (§5.1), `authorize_url(CsrfToken::new_random)`
     with the configured scopes.
  2. Persist `oauth_flows { state = csrf.secret(), provider = "github",
     expires_at = now + 10m }`.
  3. `302` to the returned GitHub authorize URL.

- `GET /auth/github/callback?code&state`
  1. Consume the `oauth_flows` row for `state`; reject if missing/expired.
  2. `exchange_code(AuthorizationCode::new(code)).request_async(&http)` to get
     the access token.
  3. Call GitHub `/user`, `/user/emails`, `/user/memberships/orgs` with the
     access token (§5.2).
  4. Provision the user (§6) and reconcile auto-groups (§9) in one transaction.
  5. Issue an `api_tokens` row; return `LoginResponse { token, expires_at }`.

- `GET /auth/whoami` — authenticated (existing `Subject` extractor); returns the
  current user's id/username/full_name. Doubles as the protected route the
  integration test hits to prove the issued token works.

### 5.1 `oauth2` client construction (v5 API)

```rust
use oauth2::{AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
             EndpointNotSet, EndpointSet, RedirectUrl, Scope, TokenResponse, TokenUrl};

type GithubClient = oauth2::basic::BasicClient<
    EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

let client = oauth2::basic::BasicClient::new(ClientId::new(cfg.client_id.clone()))
    .set_client_secret(ClientSecret::new(cfg.client_secret.clone()))
    .set_auth_uri(AuthUrl::new(cfg.auth_url.clone())?)
    .set_token_uri(TokenUrl::new(cfg.token_url.clone())?)
    .set_redirect_uri(RedirectUrl::new(cfg.redirect_url.clone())?);

// reqwest client with redirects disabled (oauth2 SSRF guidance):
let http = reqwest::ClientBuilder::new()
    .redirect(reqwest::redirect::Policy::none())
    .build()?;
```

The built client + http client live in `AppState` (or are constructed per
request from cached config). Token exchange:

```rust
let token = client
    .exchange_code(AuthorizationCode::new(code))
    .request_async(&http)
    .await?;
let access = token.access_token().secret();
```

### 5.2 Provider abstraction (future providers)

All third-party interaction sits behind a trait so additional providers are
additive and so tests can target a fake. GitHub is the only impl this milestone.

```rust
#[async_trait]
trait OAuthProvider {
    fn name(&self) -> &'static str;                       // "github"
    fn authorize(&self) -> (url::Url, CsrfToken);         // wraps authorize_url
    async fn exchange(&self, code: String) -> Result<AccessToken>;
    async fn fetch_identity(&self, t: &AccessToken) -> Result<ExternalIdentity>;
    async fn fetch_org_ids(&self, t: &AccessToken) -> Result<Vec<String>>;
}

struct ExternalIdentity {
    provider_user_id: String,        // GitHub numeric id, as text (STABLE key)
    login: String,                   // current handle (suggested username)
    full_name: Option<String>,
    avatar_url: Option<String>,
    verified_emails: Vec<String>,    // only verified addresses
}
```

`src/auth/oauth/github.rs` implements this over the `oauth2` client + `reqwest`
against `api_base_url`.

---

## 6. User provisioning (single transaction)

Given an `ExternalIdentity`:

1. **Resolve by identity:** `select user_id from user_identities where
   provider='github' and provider_user_id = $id`. If found → that user.
2. **Else link by verified email:** `select distinct user_id from user_emails
   where email = any($verified_emails)`.
   - Exactly one user → link (insert the `user_identities` row for them).
   - Zero → create (step 3).
   - More than one → ambiguous; **do not auto-merge**. Create a fresh user and
     log a warning (operators resolve merges manually). This avoids silently
     joining two accounts that happen to share a verified address.
3. **Else create:** `insert subjects(uuid, 'user')`, then `insert users(...)`
   with `username = unique_username(login)` (append `-2`, `-3`, … on collision —
   the "suggest not force" UX is a later rename endpoint), then the
   `user_identities` row.
4. **Update mutable profile:** `full_name`, `avatar_url`, `provider_login`.
5. **Upsert verified emails:** insert each into `user_emails` with `on conflict
   (email) do nothing` — never steal an address already owned by another user.
6. **Reconcile auto-groups** (§9).

`READ COMMITTED` is fine; the `group_members` writes are guarded by the
acyclicity trigger's advisory lock.

---

## 7. Deferred authorization — what the shim is, and what the rewrite entails

### 7.1 Interim shim (this milestone)

The old authorization stack queried global privilege tables that no longer
exist. Rebuilding real authorization is a milestone of its own, so for now we
keep the *plumbing* and neutralize the *decision*:

- `Subject` extractor: unchanged (token → `SubjectDetail { token_id, user_id }`).
- `DbPermSearcher::query`: returns `Authorized` for any valid subject —
  authentication implies authorization. Drop all privilege-table SQL.
- `perms.rs` `PrivilegedAction` impls and the route → `authorize(action)` →
  `Privilege` flow: unchanged, so swapping in real checks later is localized to
  `DbPermSearcher`.

This is safe **only because switchboard is pre-production** and not yet exposed.
The shim is marked with a prominent `// TODO(authz)` and tracked by the
follow-up milestone. No endpoint relies on per-resource permissions yet (the
job/supervisor handlers are still `todo!()`).

### 7.2 The full grant-based rewrite (follow-up)

Replaces the permissive shim with real checks derived from `SCHEMA.sql`:

- **Per-resource permission types** replacing string keys: align `perms.rs`
  actions with the `supervisor_permission` / `job_permission` enums.
- **A single authorization query** answering "may subject S do permission P on
  resource R?" as the disjunction of:
  1. S (or a transitively-containing group) is `owner_id` of R;
  2. a `*_grants` row grants P to S or one of those groups;
  3. S (or a group) is a member of the seeded **admins** group
     (`00000000-0000-0000-0000-000000000001`) → global authority, incl. orphans.
  The group expansion is the `with recursive my_principals` CTE (S + all groups
  it transitively belongs to) from the schema design notes.
- **Grant management endpoints:** add/revoke `*_grants` rows gated by `manage`;
  the irrevocability trigger already blocks revoking fixed grants.
- **Provenance grant insertion:** when a job is dispatched onto a group-owned
  supervisor, insert the owning group's irrevocable `stop` grant on the job
  (insert revocable, then `update … set revocable = false`).
- **Owner/orphan handling:** orphaned resources (`owner_id is null`) resolve to
  admins-only.
- **Performance:** if the recursive membership CTE shows up hot, add the
  materialized closure table noted in the schema design.

This milestone deliberately ships none of the above beyond what the schema
already enforces.

---

## 8. Local / CI testability (no third-party dependency)

Two layers, both already supported by the existing `nextest` check:

- **Database:** real ephemeral Postgres via `#[sqlx::test]` (already used across
  `src/sql/*`). Provisioning, email linking, and auto-group reconciliation run
  for real.
- **GitHub:** an in-process `wiremock::MockServer` (one dev-dependency, no
  network). The provider's `token_url` / `api_base_url` are pointed at
  `server.uri()`; the server returns canned `/login/oauth/access_token`,
  `/user`, `/user/emails`, `/user/memberships/orgs` responses.

### 8.1 Integration test outline (`tests/oauth_login.rs`)

```rust
#[sqlx::test]
async fn github_login_provisions_and_reconciles(pool: PgPool) {
    let gh = wiremock::MockServer::start().await;
    // mount canned token + /user + /user/emails + /user/memberships/orgs
    // (org id "42")

    // seed: a group G and group_auto_sources(G, 'github', '42')
    // build AppState with oauth.github.{token_url, api_base_url} = gh.uri()

    // POST/GET /auth/github/login -> capture `state` from the redirect Location
    //   (or insert the oauth_flows row directly)
    // GET /auth/github/callback?code=CANNED&state=<state>

    // assert: subjects+users row created; user_identities(github, <id>);
    //         user_emails has the verified address;
    //         group_members has (G, user, 'github_org', '42')
    // assert: returned token authenticates GET /auth/whoami (200)

    // second login with org "42" removed from the mock -> the github_org
    // membership row is gone, any manual membership untouched.
}
```

This exercises the real reqwest/JSON/transaction/reconcile path and fakes only
GitHub's HTTP responses. Runs under the existing `.#checks.nextest`.

---

## 9. Auto-group reconciliation — detailed sketch

### 9.1 Inputs

- `O_user`: the set of GitHub org ids (as text) the user currently belongs to,
  from `GET /user/memberships/orgs` (active memberships; `read:org` scope lets
  this include private memberships for the authenticated user).
- `group_auto_sources` rows with `provider = 'github'`: the (group, org)
  bindings.

### 9.2 The invariant

Reconciliation only ever inserts/deletes `group_members` rows with
`source = 'github_org'`. Rows with `source = 'manual'` are never touched, so a
manually-added member is unaffected, and a `github_org` membership is a pure
function of current org membership × bindings. `source_ref` carries the org id,
which (after §2.2) is part of the key, so multiple orgs mapping to the same
group coexist as distinct justifications.

### 9.3 Lazy path (this milestone): reconcile one user at login

Run inside the provisioning transaction, with `$1 = user subject id`,
`$2 = O_user` (text[]):

```sql
-- Desired github_org memberships for this user.
with desired as (
    select gas.group_id, gas.external_id as source_ref
    from tml_switchboard.group_auto_sources gas
    where gas.provider = 'github'
      and gas.external_id = any($2::text[])
),
-- Remove github_org rows no longer justified by a current org membership.
removed as (
    delete from tml_switchboard.group_members gm
    where gm.member_id = $1
      and gm.source = 'github_org'
      and not exists (
          select 1 from desired d
          where d.group_id = gm.group_id
            and d.source_ref = gm.source_ref
      )
    returning 1
)
-- Add newly-justified github_org rows.
insert into tml_switchboard.group_members (group_id, member_id, source, source_ref)
select d.group_id, $1, 'github_org', d.source_ref
from desired d
on conflict (group_id, member_id, source, source_ref) do nothing;
```

Notes:
- Adding a user to a group cannot create a cycle (a user is never a `group_id`),
  so the acyclicity trigger's check is trivially satisfied; it still takes its
  advisory lock, serializing concurrent membership writes harmlessly.
- `last_synced_at` on the binding is **not** updated here: the lazy path only
  observes one user, so it leaves the per-binding watermark to the periodic
  sweep (below).

### 9.4 Periodic path (follow-up, not this milestone)

A background task iterates `group_auto_sources`, and per binding fetches the
org's full member list (a GitHub app/installation token or a service account
token with org read), then reconciles every affected user the same way —
inserting/deleting only `github_org` rows — and sets `last_synced_at = now()`.
This catches membership changes for users who have not re-authenticated. It is
the only place that needs an org-wide token; the lazy path uses the logging-in
user's own token.

---

## 10. New / modified files

New:
- `src/routes/auth.rs` — login, callback, whoami handlers.
- `src/auth/oauth/mod.rs` — `OAuthProvider` trait + shared types.
- `src/auth/oauth/github.rs` — GitHub provider (oauth2 + reqwest).
- `src/sql/oauth_flow.rs` — insert/consume flow state.
- `src/sql/user.rs` — provisioning + auto-group reconcile queries.
- `tests/oauth_login.rs` — wiremock + `#[sqlx::test]` integration test.

Modified:
- `SCHEMA.sql` (+ `oauth_flows`, `group_members` PK refinement),
  `migrations/*` (regenerated), `FIXTURES.sql`.
- `src/config.rs` (+ `OAuthConfig`), `config.example.toml`.
- `src/sql/api_token.rs`, `src/auth/db.rs`, `src/auth/mod.rs`.
- `src/routes/mod.rs` (wire `/auth/*`, drop `/tokens/login`).
- `treadmill-rs/src/api/switchboard.rs` (drop `LoginRequest`, add whoami type).
- `Cargo.toml` (− argon2; + oauth2, reqwest, url; dev + wiremock).

Removed:
- `src/routes/tokens.rs`, `src/sql/perm.rs`.

---

## 11. Milestone sequence

1. Schema: `oauth_flows` + `group_members` PK refinement; regenerate migrations;
   refresh OpenAPI snapshot; rewrite `FIXTURES.sql`.
2. Make the crate compile: rewrite `sql/api_token.rs`; permissive authz shim;
   delete password login and `sql/perm.rs`; adjust API types; Cargo deps.
3. Config: `OAuthConfig`.
4. Provider trait + GitHub impl.
5. Routes + transactional provisioning + lazy auto-group reconcile + `whoami`.
6. wiremock + `#[sqlx::test]` integration test; refresh snapshot.

Build/verify requires the `.#database` devshell (Atlas for migration regen;
Postgres/`DATABASE_URL` for `sqlx` compile-time query checks).
