# Plan: wire a QEMU supervisor + zot into the `.#dev` stack (end-to-end dev testbed)

> **[REMOVE]** This is a working plan document, committed only to track the
> design while the work lands. Delete it once the feature is in and AGENTS.md /
> the OCI migration plan carry whatever is worth keeping.

## Goal

Extend the Nix dev app (`nix/apps.nix`, `nix run .#dev`) so it brings up the
*entire* system on one machine: Postgres + NATS + switchboard + console (today),
plus a **single zot registry** and a **QEMU supervisor**, bootstrapped with the
CI `tiny-efi` aarch64 fixture, so a job can be scheduled, dispatched to the
supervisor over the host WebSocket, pulled from the local store, booted under
QEMU, and torn down — exercising the real control plane on real binaries.

## Background discovered while researching (ground truth)

- **Image dispatch path.** A user registers an image with
  `POST /api/v1/images {registry, repository, manifest_digest}`. The switchboard
  *pulls the manifest by digest from that registry over the network* to validate
  it is a Treadmill artifact (`OciRegistryClient::fetch_manifest`), then records
  `(registry, repository)` location rows — never bytes. At dispatch the scheduler
  resolves the job to `ImageSpecification::Image { manifest_digest, locations:
  [{registry, repository}] }` and sends it over the host WS.
- **The supervisor ignores the dispatched `registry`.** `supervisor/qemu`
  (`main.rs` `start_job`) maps `locations` through `Location::new(loc.repository)`
  — it keeps only `repository` and resolves it against its *own local* zot
  (`oci_store.registry` + `oci_store.store_root`), pulling the manifest from the
  local daemon and then reading blob files straight off
  `store_root/<repo>/blobs/sha256/<hex>` (`oci_store.rs`). This is why **one zot
  suffices** for dev (see Decisions).
- **Loopback registries are plain HTTP.** Both `OciRegistryClient::client_for`
  (switchboard) and `OciStore::new` (supervisor) talk plain HTTP to
  `127.0.0.1`/`localhost`. So the dev zot needs no TLS; we must address it as
  `127.0.0.1:<port>` for the switchboard's loopback-detection to pick HTTP.
- **Supervisor ↔ switchboard connection.** The supervisor dials the host WS with
  the host row's `auth_token`; the switchboard authenticates it as a *host*
  (`/api/v1/hosts/{id}/connect`, `routes/hosts.rs::connect` →
  `sql::host::try_authenticate_for_host`). The single listener serves API +
  console + the host WS (`serve.rs`, one `bind_address`).
- **Pre-existing bug (fixed first, separately):** the connector dials
  `/api/v1/supervisors/{id}/connect` (`connector/ws/src/lib.rs:229`), but the
  only mounted route is `/api/v1/hosts/{id}/connect` (the supervisor→host
  rename). As-is the supervisor 404s on connect. Single dial site.
- **`tiny-efi` has no puppet.** It boots, prints `TREADMILL_OK rev=1` to serial,
  and self-shuts-down. So the job lifecycle reaches `Initializing/Booting` →
  guest exits → `Terminated`; it never reaches `Ready` (no `puppet_ready`), and
  there is no control-socket / SSH / parameters round-trip. That is the accepted
  scope for the *first* test; a puppet-bearing image comes later and extends the
  assertion to `Ready`.
- **TCG everywhere.** The boot is `qemu-system-aarch64 -M virt -cpu cortex-a57`
  with AAVMF, no acceleration — exactly the existing `qemu-boot` Nix check
  (`nix/checks.nix`), *not* the x86 `qemu-kvm`/`q35` in
  `supervisor/qemu/config.example.toml`. No `/dev/kvm` assumption.
- **`tml` CLI is a stub** (`cli/src/main.rs` prints "Hello World!"). Any test
  driver is raw HTTP (curl/jq), as in `.github/workflows/e2e-tests.yml`.
- **OAuth provisioning** (`sql::user::provision_user`): resolve by
  `(provider, provider_user_id)` in `user_identities`; else link by a *single*
  shared **verified** email in `user_emails`; else create a new user. `alice` is
  the built-in mock **admin** identity (`auth/oauth/mock.rs`).
- **Sandbox TCP claim is wrong (here).** AGENTS.md §2 says daemons that bind TCP
  are killed in the restricted sandbox. Empirically disproven in this
  environment: a `socat TCP-LISTEN:127.0.0.1:8731` bound, accepted a loopback
  connection, and round-tripped. So verification can run the real daemons. (We
  leave AGENTS.md's note alone for now — it may still hold under a *more*
  restricted sandbox; we just don't rely on it.)

## Decisions

- **Single zot for dev.** Push `tiny-efi` directly into one zot; point both the
  switchboard catalog location *and* the supervisor's `store_root`/`registry` at
  it. The two-tier (canonical + per-server pull-through cache) split buys nothing
  here because the supervisor reads its local store directly and ignores the
  dispatched registry; the sync/pull-through path is already covered hermetically
  by the `oci-store` check (`pull_through_caches_from_upstream`).
- **No scaffold refactor.** Keep the `.#dev` body inline. We will generate e2e
  test scripts later via Nix templating that reuses this same scaffold; no
  premature factoring.
- **Seed once.** Apply the dev seed a single time, sentinel-guarded, so
  mock-OAuth users created by interactive logins survive restarts and the host /
  token rows are stable.
- **One dev admin user, login-linked.** Seed a single user (`alice`) that owns
  the host and the API token, is in the `admins` group, and carries a
  `user_identities (mock, alice)` row + a verified `alice@example.test` email, so
  a mock login as `alice` resolves to exactly this subject with full permissions
  immediately.
- **Reuse the known FIXTURES token material.** The host `auth_token`
  (bearer `OCkr…`) already matches `supervisor/qemu/config.example.toml`; reuse
  it and the host id `7d55ec6d-15e7-4b84-8c04-7c085fe60df4` as the supervisor id.
  Reuse the FIXTURES API token (bearer `B1oy2ko1…`) under the dev user for the
  headless smoke test.

## Work breakdown (commits)

### Commit 0 — this plan (`[REMOVE]`-prefixed)

### Commit 1 — fix the host WS connect URL (standalone, lands first)
- `connector/ws/src/lib.rs:229`: `/api/v1/supervisors/{}/connect` →
  `/api/v1/hosts/{}/connect`. Update the matching stale comment at
  `switchboard/src/routes/mod.rs:123` (`GET /hosts/{id}/connect`). The other
  `/supervisors/...` comments are future routes (status/current-job/new) and stay.
- Verify: `cargo build -p treadmill-ws-connector` + `clippy`.

### Commit 2 — dev seed + zot + image bootstrap in `.#dev` (Linux-only additions)
Extend the existing inline `dev` app. New env/ports: `TML_ZOT_PORT` (default
e.g. 5000). New runtime inputs, gated `lib.optionalAttrs pkgs.stdenv.isLinux`
(the `tiny-efi-image-layout` package is Linux-only): `self'.packages.zot`,
`pkgs.skopeo`, `pkgs.qemu`, `pkgs.jq`, `pkgs.curl`. Pass in via substitution: the
fixture layout path and `TML_AAVMF_CODE` / `TML_AAVMF_VARS` (from `pkgs.qemu`
share, as in the `qemu-boot` check).

- **zot**: render `$cfg_dir/zot.json` (`rootDirectory = $state_dir/zot`, plain
  HTTP `127.0.0.1:$zot_port`, `dedupe: true`), start it, wait until `/v2/` is up.
- **push image (once)**: `skopeo --insecure-policy copy --dest-tls-verify=false
  oci:<fixture> docker://127.0.0.1:$zot_port/treadmill/tiny-efi:latest`.
- **dev seed (once, sentinel `$state_dir/db-seeded`)**: a generated SQL file
  applied after the switchboard has migrated. Creates subject+user `alice`
  (admin via `group_members` → `admins`), `user_identities (mock, alice)`,
  verified `user_emails` `alice@example.test`, the host `7d55ec6d…` owned by
  `alice` with the FIXTURES `auth_token` and tag `host:7d55ec6d…`, and the
  FIXTURES API token under `alice`. (Adapted from `FIXTURES.sql`; does **not**
  `TRUNCATE`.)
- **register image**: read the manifest digest from `<fixture>/index.json` via
  `jq`, then `POST /api/v1/images {registry:"127.0.0.1:$zot_port",
  repository:"treadmill/tiny-efi", manifest_digest}` with the seeded API token;
  tolerate an already-registered response for idempotent re-runs.

### Commit 3 — QEMU supervisor in `.#dev` (Linux-only)
- Generate `$cfg_dir/supervisor.toml`:
  - `base.supervisor_id = 7d55ec6d…`, `coord_connector = ws_connector`.
  - `ws_connector.token = <FIXTURES host bearer>`,
    `switchboard_uri = "ws://127.0.0.1:$sb_port"`.
  - `oci_store.registry = "127.0.0.1:$zot_port"`,
    `oci_store.store_root = "$state_dir/zot"`.
  - `qemu`: `qemu-system-aarch64`, `qemu-img`, `-M virt -cpu cortex-a57` (TCG),
    AAVMF pflash (code read-only from store; vars copied writable per-job),
    `-device virtio-blk-device,drive={disk_node}`, serial wired for capture,
    `working_disk_max_bytes` = the 16 MiB fixture layer size,
    `state_dir = $state_dir/supervisor`.
  - A generated aarch64 `start_script` (modeled on
    `supervisor/qemu/nix_ovmf-vars_start_script.sh`) that copies `$TML_AAVMF_VARS`
    → `$TML_JOB_WORKDIR/AAVMF_VARS.fd` writable.
- Start the supervisor (`self'.packages.treadmill-qemu-supervisor`); add to the
  cleanup pid list. Start ordering:
  postgres → zot → push → nats → switchboard(+migrate) → seed → register →
  supervisor → console → smoke stage → `wait`.
- Update the banner.

### Commit 4 — API smoke stage at the end (prints the commands it runs)
After the stack is up, run a small HTTP smoke test against the API using the
seeded token, echoing each `curl` it issues: register/list image (idempotent),
submit a tiny-efi job targeting tag `host:7d55ec6d…` with the registered digest,
and poll `/jobs/{id}` a few cycles to show it dispatched and reached `Booting`.
(See open question on how far to drive / whether to gate behind an env var.)

## Verification
- `nix build` the dev app program (runs **shellcheck** via `writeShellApplication`).
- `nix flake check` evaluation; connector `cargo build` + `clippy` for commit 1.
- The boot path itself is already proven by the `qemu-boot` check.
- Live end-to-end (`nix run .#dev`) now *can* be run here (TCP daemons work),
  modulo build time; otherwise the user runs it.
- Update AGENTS.md §2 to mention zot + the qemu supervisor in `.#dev`.

## Open questions (do not block commits 0/1)
1. **Smoke stage behavior** (commit 4): auto-submit a job at startup and poll
   (adds a ~tens-of-seconds TCG boot to startup), vs. just print ready-to-paste
   commands, vs. gate behind `TML_DEVSTACK_SMOKE=1`. Leaning: run it but only
   poll until `Booting`/dispatched (don't block on full termination), printing
   every command.
2. **Dev user** — confirm a single **admin** `alice` (mock-login = full access)
   is the right UX, vs. also seeding a non-admin owner to exercise
   ownership-based permissions.
