# Host/Supervisor Log Streaming — Implementation Plan

**Status:** in progress — phases 0–3 and 4a landed on `dev/switchboard-refactor`
· **Date:** 2026-06-13

> **Progress / revisions (2026-06-13).** Phases 0, 1, 2, 3a, 3b, and 4a are
> committed (write path end-to-end, plus the switchboard read-token route). The
> `tml log` CLI originally planned for Phase 4 is **dropped**: the read-path
> client will be the **web console** (`nats.ws` + xterm.js), which is the
> remaining work. See §8 Phase 4 for the revised breakdown.

> This document is a **one-time implementation roadmap**, self-contained so a new
> contributor (human or agent) can pick up the work without the design
> conversation that produced it. As with the other plans under `doc/`, the
> durable *specification* lives in the code once written: wire types carry
> Rustdoc + committed JSON-Schema snapshots, and this file may be deleted or
> archived once the phases land.

---

## 1. What we are building and why

Treadmill supervisors run workloads (qemu VMs via `supervisor/qemu`, netbooted
boards via `supervisor/nbd-netboot`) that produce **streaming console output**:
qemu's stdout/stderr and a serial console. Today this output is *not captured* —
qemu's stdout/stderr are inherited to the supervisor's own terminal
(`supervisor/lib/src/launcher.rs:207-208`) and the serial console is wherever the
job's qemu args point it.

We want that output **streamed off the host, timestamped, tagged by channel, to a
central service** that:

- persists each job's log and can **replay** it (download the whole thing later);
- supports **live tailing** over WebSocket (for the `tml` CLI and the web console,
  which will render it through an xterm.js terminal emulator);
- gates writes with a **token the switchboard mints per job**, gates reads with a
  **token scoped to that job**;
- keeps the log bytes **off the switchboard**, which runs on fly.io where storage
  is expensive.

### Chosen technology: NATS + JetStream (self-hosted)

After evaluating log-aggregation backends (Grafana Loki, etc.) against message
brokers, **NATS with JetStream** was chosen. Every requirement pointed at a
broker rather than a log aggregator:

- **Raw byte fidelity** — the serial console emits ANSI escapes, partial lines,
  and possibly non-UTF8; a line-oriented aggregator would mangle it. NATS carries
  opaque byte frames.
- **Replay + live tail only** (no server-side search) — JetStream unifies both as
  one consumer (deliver-all, then follow); a query engine would be dead weight.
- **Storage must enforce the per-job token natively** — NATS decentralized JWT
  auth verifies pub/sub scope *in the server*. (Loki's multi-tenancy is a trusted
  header, which would require building a bespoke auth proxy — exactly what we want
  to avoid.)
- **Storage cost is a non-issue** — a dedicated VM with GBs of disk hosts NATS;
  JetStream's local file store is the right fit, so object-storage tiering is
  unnecessary.

NATS is self-hosted on its **own VM, separate from the fly.io switchboard**, so
bulk byte traffic never touches the control plane. The server is a single static
Go binary; the JS browser client (`nats.ws`) and Rust client (`async-nats`) both
speak it.

---

## 2. Architecture

```
 supervisor (qemu / nbd-netboot)                NATS + JetStream (own VM)
 ┌───────────────────────────────┐             ┌──────────────────────────┐
 │ qemu stdout ─┐                 │  publish    │ stream "job-<id>"        │
 │ qemu stderr ─┤ capture → spill │  (bearer    │  subjects logs.<id>.*    │
 │ serial(sock)─┘   file → ship   │───JWT,─────▶│  no MaxAge (never expires)│
 │            (ack-tracked)       │   PUB scope)│                          │
 └───────────────────────────────┘             └──────────┬───────────────┘
        ▲ StartJobMessage                                  │ SUB (bearer JWT)
        │ {nats_url, subject_prefix, write_token}          │ tail + replay
 ┌──────┴────────────────────────┐             ┌───────────┴──────────────┐
 │ switchboard (fly.io)          │  mint read  │ tml CLI (async-nats)     │
 │  - create stream at dispatch  │  token ◀────│ web console (nats.ws +   │
 │  - mint write JWT (per job)   │  (job:read) │   xterm.js)              │
 │  - mint read JWT (on demand)  │             └──────────────────────────┘
 └───────────────────────────────┘
```

- **Subject:** `logs.<job-id>.<channel>`, where `<channel> ∈ {qemu-stdout,
  qemu-stderr, serial}`. Job and channel fall out of the subject hierarchy — no
  envelope parsing to route or filter.
- **Stream:** one JetStream stream **per job**, capturing `logs.<job-id>.>`, with
  **no `MaxAge`** (it never expires in NATS). The switchboard creates it at
  dispatch.
- **Message:** payload = raw byte chunk (flush on ~16 KB or ~100 ms). Metadata in
  NATS **headers**, not the payload:
  - `Tml-Ts` — capture timestamp taken **on the supervisor** (NTP wall-clock, ns).
    *Not* JetStream's server-side ingest time, which is skewed by buffering/retries.
  - `Nats-Msg-Id` — `<source>-<seq>`, so resume-after-reconnect is idempotent
    (JetStream dedups within its window).
- **Auth:** the switchboard holds a NATS **account signing seed** (secret). Per
  request it generates an ephemeral user nkey and mints a **bearer** user JWT
  signed by that seed:
  - *write* (supervisor): scoped `pub` to `logs.<job-id>.>`, long-lived,
    re-minted on every dispatch (see §4 decision 2), delivered inside
    `StartJobMessage` over the existing authenticated switchboard↔supervisor WS.
  - *read* (clients): scoped `sub` to `logs.<job-id>.>`, short-lived, re-minted on
    reconnect, no revocation list.
  Both are **bearer** tokens, so the client connects with just the JWT string (no
  nkey seed shipped anywhere). The NATS server rejects anything outside the
  granted scope — this is the "storage enforces the token" requirement.

---

## 3. Locked decisions

| # | Decision | Rationale |
|---|---|---|
| Stack | Self-hosted NATS + JetStream, own VM, separate from switchboard | Native scoped-JWT auth, raw-byte fidelity, unified replay+tail, keeps bytes off fly.io |
| Stream topology | One stream **per job**, captures `logs.<job-id>.>` | "A job's logs are an object with a lifecycle"; trivial per-job purge later (delete stream) |
| Retention in NATS | **No `MaxAge`** — streams never expire | GC/long-term archival handled separately, later (see §7 Out of scope) |
| Channels | `qemu-stdout`, `qemu-stderr`, `serial` | Serial captured via a qemu `-chardev socket`; the channel is just called `serial` |
| Write token | Per-job **bearer** JWT, scoped `pub logs.<job-id>.>`, **re-minted each dispatch** (not persisted) | Reconcile re-sends `StartJob` idempotently; re-minting avoids a DB column, all tokens have identical scope |
| Read token | Short-lived **bearer** JWT, scoped `sub logs.<job-id>.>`, re-minted on reconnect, no revocation | Leaked read token exposes only that one job until expiry; simplest static NATS account |
| JWT minting | The **`nats-jwt`** crate (+ `nkeys`) | Less bespoke code than hand-rolling the Ed25519 claim encoder |
| Durability | **Must-not-lose**: supervisor spills to a local file, ships with publish-ack tracking, resumes after downtime | Serial has no backpressure (slow drain loses bytes at the source); NATS is the only copy |
| Old `ConsoleLog` event | **Removed entirely** | Superseded by this design; it was lossy, untyped (no channels), and never actually produced |
| Browser transport | `nats.ws` over `wss://` + xterm.js, origin-allowlisted server-side | WebSocket needs no CORS headers; NATS validates `Origin` via `allowed_origins` |

---

## 4. Remaining open decisions

1. **`nbd-netboot` serial capture mechanism.** The qemu supervisor captures
   serial via a `-chardev socket`. The netboot supervisor taps a *real board's*
   serial line — mechanism (direct serial device, `ser2net`/`conserver`, etc.) is
   TBD. Phase 3 can land qemu-only first; netboot follows.
2. **Object storage / GC** — deliberately deferred, see §7.

---

## 5. Repository touchpoints (grounding)

Confirmed by inspection of the current tree:

- **No schema work needed for this feature.** Streams are keyed by the existing
  `jobs.job_id`; the write token is ephemeral (re-minted, not stored); read
  authorization reuses the existing `job_permission::read` variant and
  `job_grants` table (`switchboard/SCHEMA.sql:657,670`). The jobs table comment
  already notes captured output is *not* stored in the DB (`SCHEMA.sql` ~`:505`).
- **Console capture does not exist yet.** `SupervisorJobEvent::ConsoleLog`
  (`treadmill-rs/src/api/switchboard_supervisor.rs:350`) is defined but never
  produced; qemu stdout/stderr are inherited
  (`supervisor/lib/src/launcher.rs:207-208`). We build capture from scratch.
- **Wire types:** `StartJobMessage`
  (`treadmill-rs/src/api/switchboard_supervisor.rs:169`); the directional enums
  `SwitchboardToSupervisor`/`SupervisorToSwitchboard` (`:480`/`:509`);
  `SupervisorJobEvent` (`:321`, contains the `ConsoleLog` variant to delete).
  Protocol-schema snapshot guard: `treadmill-rs/tests/protocol_schema.rs`
  (regenerate with `UPDATE_SCHEMA=1 cargo test -p treadmill-rs`).
- **Dispatch path:** `build_start_job_message` (`switchboard/src/sql/job.rs:640`)
  builds the message; reconcile sends it and **re-sends `StartJob` idempotently**
  on re-dispatch (`switchboard/src/supervisor_ws_worker.rs:466-472`).
- **Supervisor launch + capture seam:** `ProcessLauncher` / `WorkloadProcess`
  traits (`supervisor/lib/src/launcher.rs:68-101`, real impl `:197-218`); qemu
  launch path (`supervisor/qemu/src/main.rs` ~`:760-825`). The supervisor talks
  to the switchboard via the `SupervisorConnector` trait
  (`treadmill-rs/src/connector.rs`); `StartJobMessage` arrives through
  `Supervisor::start_job`.
- **Switchboard config:** `SwitchboardConfig` + figment loader, `TML_`-prefixed
  env, `__` nesting (`switchboard/src/config.rs:9` and `load_configuration` at the
  bottom). Secrets go through env vars, not on-disk config (project convention).
  The existing opaque API token (`switchboard/src/auth/token.rs`, 128 random
  bytes) is **unrelated** to NATS JWTs — do not reuse it.
- **Auth/permissions:** `switchboard/src/auth/` (`engine.rs`, `extract.rs`); read
  authz reuses `job_permission::read`.
- **OpenAPI snapshot guard:** `switchboard/tests/openapi_spec.rs` (regenerate with
  `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard --test openapi_spec`).
- **Dev/CI infra:** dev shells `nix/devshells.nix` (zot added at `:28`); the
  `nix run .#dev` full stack `nix/apps.nix:53+` (Postgres + switchboard +
  console); external-daemon packaging precedent `nix/pkgs/zot.nix`; hermetic
  checks `nix/checks.nix`; check fileset `nix/lib.nix`.

---

## 6. Testing & the sandbox constraint

**`nats-server` is TCP/WebSocket only — it has no unix-socket client transport.**
Per `AGENTS.md` §2, daemons that bind a TCP port are killed in the restricted
sandbox (zot exits 144). Postgres dodges this with a unix socket; **NATS cannot.**

Consequence, mirroring the `oci-store`/registry situation exactly:

- Anything that needs a live NATS server must be a **hermetic Nix check**
  (`nix/checks.nix`) and, if written as a Rust test, **`#[ignore]`d** so the
  no-daemon `nextest` check skips it. You **cannot** run the broker in the inner
  loop or the sandbox.
- Pure logic (the publisher's spill/ack/resume state machine, JWT minting and
  scope assembly) should be unit-testable **without** a daemon, using stubs — keep
  that logic separable so most of it runs under plain `nextest`.

Standard verification per change (`AGENTS.md` §3): scoped `cargo build` +
`cargo clippy` on changed crates, `nix fmt -- <files>`, then the relevant
hermetic check(s). Switchboard DB tests also run via `nextest-db`.

---

## 7. Out of scope (deliberately deferred)

- **Object storage, long-term archival, and garbage collection.** Streams persist
  in NATS with no `MaxAge`. There is **no archiver** and **no schema change** in
  this plan. *Operational note:* without the deferred GC, streams accumulate
  indefinitely — acceptable for now given the dedicated, generously-sized VM. A
  later effort will consume finished-job streams into long-term storage and delete
  them from NATS; design that separately.
- **The web console's xterm.js UI** beyond confirming `nats.ws` connectivity — the
  CLI read path is the MVP; the browser terminal can follow.

---

## 8. Phases

Each phase builds and passes its relevant checks on its own (`AGENTS.md` §8: one
focused commit per coherent piece).

### Phase 0 — Infra, dependencies, NATS auth bootstrap

The real starting point; nothing compiles against NATS until this exists.

- **Package the daemon.** `nats-server` is in nixpkgs. Add it to the `default`
  dev shell (`nix/devshells.nix`, beside `cmn.zot` at `:28`) and to the
  `nix run .#dev` stack (`nix/apps.nix:53+`) so the dev stack runs a live broker
  alongside Postgres/switchboard/console. Enable JetStream (file store under the
  devstack state dir) and a `websocket {}` listener.
- **NATS decentralized-auth bootstrap.** Generate a one-time operator→account
  hierarchy (via `nsc`). Commit the **public** operator + account JWTs into the
  nats-server config (`resolver: MEMORY`). The switchboard holds the **account
  signing seed** as a secret. The server then trusts any user JWT the switchboard
  signs. Document the regeneration procedure.
- **Dependencies.** Add `async-nats` (client + JetStream), `nats-jwt`, and
  `nkeys` to the workspace. (None are present today.)
- **Switchboard config.** Add a `LogStreamingConfig` section to `SwitchboardConfig`
  (`switchboard/src/config.rs:9`): NATS client URL, JetStream domain (if any), and
  the account signing seed — secret, supplied via `TML_LOGSTREAMING__ACCOUNT_SEED`
  (on-disk config carries only non-secret values, per project convention).
- **Verify:** dev stack boots NATS; a throwaway `nats pub`/`nats sub` round-trips
  against it.

### Phase 1 — Shared wire types

In `treadmill-rs/src/api/switchboard_supervisor.rs`:

- Add a `LogChannel` enum: `QemuStdout`, `QemuStderr`, `Serial` (serialize to the
  subject tokens `qemu-stdout` / `qemu-stderr` / `serial`).
- Add `log_streaming: Option<LogStreamingDispatch>` to `StartJobMessage` (`:169`),
  where `LogStreamingDispatch = { nats_url: String, subject_prefix: String /*
  "logs.<job_id>" */, write_token: String /* bearer JWT */ }`. `Option` keeps the
  change backward-compatible and lets a deployment run with log streaming off.
- **Remove `SupervisorJobEvent::ConsoleLog`** (`:350`) and every reference to it.
- Regenerate the protocol-schema snapshot:
  `UPDATE_SCHEMA=1 cargo test -p treadmill-rs` (guard:
  `treadmill-rs/tests/protocol_schema.rs`). Confirm the diff is the intended
  additive `StartJobMessage` field + the `ConsoleLog` removal.

### Phase 2 — Switchboard: mint token + create stream at dispatch

- **JWT minting module** under `switchboard/src/` (sibling to, *not* part of,
  `auth/token.rs`): given a job id and a scope (`pub`/`sub`), generate an
  ephemeral user nkey and mint a **bearer** user JWT scoped to
  `logs.<job-id>.>`, signed with the account signing seed via `nats-jwt`.
- **Stream lifecycle.** In `build_start_job_message`
  (`switchboard/src/sql/job.rs:640`), before the message is returned: create the
  per-job JetStream stream (`logs.<job-id>.>`, **no `MaxAge`**) **idempotently**
  (it may already exist from a prior dispatch — see the reconcile re-send at
  `supervisor_ws_worker.rs:466`), and mint a fresh write token. Populate
  `StartJobMessage.log_streaming` with `{nats_url, subject_prefix, write_token}`.
- **Tests:** a `#[sqlx::test]` (marked
  `#[ignore = "requires a database; run via the nextest-db check"]`) asserting
  dispatch populates `log_streaming` with a token of the correct scope (decode the
  JWT claims; no live NATS needed). The live stream-creation assertion goes in a
  hermetic Nix check (§6). If `query!`/`query_as!` macros change, regenerate the
  root `.sqlx` cache (`AGENTS.md` §4).

### Phase 3 — Supervisor: capture + durable publish

- **Capture seam.** Extend `ProcessLauncher::spawn` / `WorkloadProcess`
  (`supervisor/lib/src/launcher.rs:68-101,197-218`) to pipe stdout/stderr and
  expose them as `AsyncRead` (instead of `Stdio::inherit()`). Keep the trait
  stub-friendly for tests.
- **Serial.** Route qemu's serial console to a `-chardev socket` in the qemu args
  (`supervisor/qemu/src/main.rs` launch path ~`:760-785`) and read that socket as
  the `serial` channel. (`nbd-netboot` serial is open decision §4.1 — qemu-only
  may land first.)
- **Publisher** (in `supervisor/lib`, shared by both supervisors): per job, drain
  each channel **fast** into a local **spill file**; ship from the spill file to
  JetStream with publish-ack tracking; advance the spill cursor only on ack;
  resume from the cursor after NATS downtime (must-not-lose). Tag each message
  with subject `logs.<job-id>.<channel>` and headers `Tml-Ts` (capture ns) +
  `Nats-Msg-Id` (`<source>-<seq>`). Connect using the bearer `write_token` from
  `StartJobMessage`.
- **Tests:** publisher spill/ack/resume as pure unit tests against a stub (no
  daemon); end-to-end capture→NATS as a hermetic Nix check.

### Phase 4 — Client read path

- **4a — Switchboard read-token API. ✅ DONE** (commit `ce2dc4c`). Route
  `POST /api/v1/jobs/{id}/log-token` (`switchboard/src/routes/jobs.rs::log_token`)
  mints a **bearer** `sub` JWT for `logs.<job-id>.>`, gated on the job `read`
  permission via `auth::engine::can_access_job` (owner / explicit grant / global
  admin → else `403`); returns `503` when log streaming is unconfigured. Response
  is the shared DTO `treadmill_rs::api::switchboard::jobs::LogStreamCredentials`
  `{ nats_url, subject: "logs.<id>.>", token, expires_in_secs }` — a **relative**
  `expires_in_secs` (5-min TTL) rather than an absolute `expires_at`, to dodge
  clock skew. OpenAPI snapshot regenerated; tests in
  `switchboard/tests/job_routes.rs`.
- **4b — Web console (remaining work; the CLI is dropped).** The read client is
  the **web console**, not a `tml log` CLI: `nats.ws` over `wss://`, the per-job
  subject piped into xterm.js for ANSI rendering. The console calls
  `POST /jobs/{id}/log-token` for credentials, then opens a JetStream consumer
  (replay history, then follow live). Set the NATS `websocket` listener's
  `allowed_origins` to the console's origin (no CORS headers needed — the
  WebSocket handshake's `Origin` is validated server-side; there is no preflight).
  When the console's typed switchboard client (`treadmill_rs::api::switchboard::client`)
  grows a `log_token` method, also register it there to keep route/payload parity.

---

## 9. Definition of done (MVP)

A dispatched qemu job streams `qemu-stdout`, `qemu-stderr`, and `serial` into a
per-job JetStream stream using a switchboard-minted write token; a user with
`job:read` can obtain a read token and both **download** the full log and **tail**
it live via `tml log` (and, stretch, the browser console). No bytes traverse the
switchboard; the old `ConsoleLog` event is gone. GC/archival remains out of scope.
