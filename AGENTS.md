# AGENTS.md — Treadmill developer & agent guide

This file documents how the Treadmill repository is structured and how to build,
test, and change it. It is written for any contributor — human or AI coding
agent (Claude Code, Cursor, etc.). Tool-specific assistants may also read
`CLAUDE.md`; this file is the canonical source and `CLAUDE.md`, if present,
should just point here.

Keep this file up to date when you change the build, the dev shells, the
migration workflow, or the `.sqlx` strategy.

> **Docs are in flux.** The project is mid-refactor; prose docs (including
> `README.md` and the architecture description below) may be **outdated**, and
> authoritative docs will be written once the refactor settles. Treat the
> **code, the flake, and the migration/design notes under `doc/` as ground
> truth** over any narrative description. This file focuses on the
> build/test/dev *workflow*, which is current.

---

## 1. What Treadmill is

Treadmill is a **distributed hardware testbed**: it schedules jobs onto real or
virtual hardware "supervisors" and gives users an image-based, reproducible way
to run workloads on them. `README.md` has an architecture diagram (possibly
stale — see the note above); `doc/` holds the active design notes. The major
components, mapped to crates (crate boundaries are current; the narrative may
lag the refactor):

| Component | Crate(s) | Role |
|---|---|---|
| **Switchboard** (central coordinator) | `switchboard/` | Auth, job scheduling, the image catalog, the supervisor WebSocket protocol server. Postgres-backed. |
| **Supervisors** | `supervisor/qemu`, `supervisor/nbd-netboot`, `supervisor/lib` | Run jobs on a host: fetch images from the local OCI store, build qcow2 backing chains, boot qemu / netboot a board. |
| **Puppet** | `puppet/` | In-guest agent that talks to its supervisor over a control socket. |
| **Connector / control-socket** | `connector/ws`, `control-socket/tcp/*` | Transport glue between the above. |
| **CLI** | `cli/` | `tml` user-facing command-line client. |
| **Shared library** | `treadmill-rs/` | Wire types (client API + supervisor protocol), the OCI image model, and shared utilities used by everything else. |

Active work: the **OCI image migration** (home-grown TOML image format → OCI
images + an OCI registry). The roadmap and current status live in
`doc/oci-image-migration-plan.md` — read it before touching anything under
`treadmill-rs/src/image/`, the supervisor store/launch paths, or the switchboard
image catalog.

It is a Cargo workspace (`resolver = "2"`). Members: `cli`, `connector/ws`,
`control-socket/tcp/client`, `control-socket/tcp/server`, `puppet`,
`supervisor/lib`, `supervisor/nbd-netboot`, `supervisor/qemu`, `switchboard`,
`treadmill-rs`. The `nix/fixtures/tiny-efi` crate is **excluded** from the
workspace (it builds for `aarch64-unknown-uefi` via its own lockfile).

---

## 2. Toolchain & development environment (Nix)

**Everything is driven through the Nix flake.** The pinned Rust toolchain and all
external tools (Postgres, Atlas, zot, skopeo, qemu, AAVMF firmware, …) live in
the dev shells — they are **not** on a bare `PATH`. Always run commands inside a
dev shell:

```bash
nix develop --command bash -c 'cargo build -p treadmill-rs'      # default shell
nix develop .#database --command bash -c './switchboard/migrate.sh -v'  # DB shell
```

### Dev shells (`nix/devshells.nix`)

- **`default`** — the everyday shell. Has the Rust toolchain (`cargo`,
  `clippy`, `rustfmt`), `nixfmt`, plus the heavier externals used by tests:
  `zot`, `skopeo`, `qemu`, the AAVMF firmware env vars (`TML_AAVMF_*`) for
  the boot test, and the NATS log-streaming tools (`nats-server`, `nsc`, `nats`).
  `SQLX_OFFLINE` is effectively on here (no DB), so builds use the
  committed `.sqlx` query cache (see §4).
- **`database`** — adds `postgresql`, `atlas`, `sqlx-cli`, `sql-formatter`. On
  entry it **spins up an ephemeral Postgres** on a unix socket and exports
  `PGHOST`, `PGUSER`, `PGDATABASE=postgres`, and `DATABASE_URL`. Use this shell
  for anything that needs a live database: running `#[sqlx::test]` tests,
  regenerating `.sqlx`, or authoring migrations. The cluster is torn down when
  the shell's process exits.

### Sandbox caveat (important for agents)

If you are running in a restricted sandbox, **daemons that bind a TCP port are
killed** (e.g. `zot` exits 144; `nats-server` has no unix-socket transport and is
likewise unusable). Postgres in the `database` shell uses a **unix socket**, so it
survives. Practical consequence: you cannot run `zot`/registry or NATS broker
experiments locally — verify any code that touches the OCI registry/store or the
NATS log stream **only** via its hermetic Nix check (§3), never by running the
daemon by hand. (`nsc`, which only writes key/JWT files, and `nats-server -t -c`,
which validates a config without binding, do both run fine in the sandbox.)

---

## 3. Building, testing, and the Nix checks

Build / lint / test a specific crate (fast inner loop):

```bash
nix develop --command bash -c 'cargo build -p treadmill-switchboard'
nix develop --command bash -c 'cargo clippy -p treadmill-switchboard --all-targets'
nix develop --command bash -c 'cargo test  -p treadmill-rs'
```

**Clippy note:** the workspace-wide `clippy --all-targets -- -D warnings` is
green at base. Keep it that way: confirm your changes add no new warnings before
committing.

The authoritative gates are the **flake checks** (`nix/checks.nix`), run
hermetically. Build one with:

```bash
nix build .#checks.x86_64-linux.<name> -L
```

| Check | What it does |
|---|---|
| `clippy` | `clippy --all-targets --all-features -- -D warnings` over the workspace. |
| `nextest` | Workspace tests **without** a database. `#[sqlx::test]`/DB tests must be `#[ignore]`d so this passes them over (see §6). |
| `nextest-db` | The DB-backed tests: spins up ephemeral Postgres and runs `--run-ignored only`. **This is where `#[ignore]`d `#[sqlx::test]` and DB-backed unit tests actually execute.** |
| `switchboard-migrations-consistency` | Asserts `switchboard/migrations/` replays to exactly `switchboard/SCHEMA.sql` (same as `./migrate.sh -v`). |
| `tiny-efi-image`, `oci-store`, `lease`, `chain-assembly`, `qemu-boot` | OCI-migration integration checks (build the `tiny-efi` fixture, exercise the OCI store / GC leases / qcow2 backing-chain assembly / an aarch64 boot). Linux-only; skip cleanly when their external tool/fixture env vars are unset. |

After a change, the usual verification set is: scoped `cargo build` + `clippy`,
then the relevant hermetic check(s) — for switchboard work that's `nextest-db`
and `switchboard-migrations-consistency`.

---

## 4. The `.sqlx` query cache strategy (read this before touching SQL)

`switchboard` uses `sqlx`'s **compile-time-checked** `query!`/`query_as!` macros.
Offline builds (the default shell and all Nix checks set `SQLX_OFFLINE=true`)
resolve those macros against a committed **`.sqlx` query cache** instead of a
live database. If you add or change a `query!`, you must regenerate the cache or
the offline build breaks with *"no cached data for this query."*

### There is ONE `.sqlx` directory: the workspace root `/.sqlx`

`cargo sqlx prepare --workspace` writes a single cache at the workspace root, and
the sqlx macros resolve it from there (they walk up from `CARGO_MANIFEST_DIR` to
the workspace root) — so a crate-local `switchboard/.sqlx` is **not** needed even
for a `cd switchboard && cargo build`. The Nix checks' source fileset
(`nix/lib.nix`) ships only the root cache.

Regenerate it from a clean build with the `--all-targets` scope (required to
capture `#[cfg(test)]` query macros, e.g. in `supervisor_ws_worker.rs`):

```bash
nix develop .#database --command bash -c '
  set -e
  cd switchboard && sqlx migrate run --source migrations && cd ..   # apply migrations to $DATABASE_URL
  unset SQLX_OFFLINE
  cargo clean -p treadmill-switchboard            # force a full recompile so ALL queries are collected
  cargo sqlx prepare --workspace -- --all-targets # writes the ROOT /.sqlx
'
```

**Gotcha:** `cargo sqlx prepare` only collects queries from files it actually
(re)compiles. Run against a warm `target/`, it silently **drops** the cache
entries for unchanged queries, so the regenerated cache is incomplete. Always
`cargo clean -p treadmill-switchboard` first, and verify with an offline build:

```bash
nix develop --command bash -c 'SQLX_OFFLINE=true cargo build --workspace --all-targets'
```

---

## 5. Database migrations (switchboard)

`switchboard/SCHEMA.sql` is the **hand-written source of truth** for the schema.
The ordered files in `switchboard/migrations/` are **derived** from it by Atlas
and are what `sqlx::migrate!()` applies at runtime. `switchboard/migrate.sh`
keeps the two in sync — run it inside the `.#database` shell:

```bash
nix develop .#database --command bash -c './switchboard/migrate.sh -c <name>'  # author a migration from the SCHEMA.sql diff
nix develop .#database --command bash -c './switchboard/migrate.sh -v'         # verify migrations/ reproduces SCHEMA.sql
nix develop .#database --command bash -c './switchboard/migrate.sh -r'         # re-hash atlas.sum after a manual edit
```

Workflow for a schema change: **edit `SCHEMA.sql`**, run `migrate.sh -c <name>`
to generate the migration + update `atlas.sum`, run `migrate.sh -v`. Do **not**
hand-write migration files. The `switchboard-migrations-consistency` check
enforces this.

---

## 6. Testing conventions

- **Unit tests** live in-crate (`#[cfg(test)] mod tests`). Pure tests run under
  the plain `nextest` check.
- **DB-backed tests** use `#[sqlx::test]` (provisions a fresh per-test database
  from `DATABASE_URL`). sqlx 0.9 does **not** auto-`#[ignore]` these, but the
  Nix split requires it: the no-DB `nextest` check would fail on them, and the
  `nextest-db` check runs `--run-ignored only`. **Therefore every `#[sqlx::test]`
  / DB-backed test MUST be marked `#[ignore = "requires a database; run via the
  nextest-db check"]`.** Run them locally with:
  ```bash
  nix develop .#database --command bash -c 'cargo test -p treadmill-switchboard -- --ignored'
  ```
- **HTTP route tests** mirror `switchboard/tests/user_routes.rs`: drive the real
  router over a loopback socket against ephemeral Postgres, using `wiremock` for
  external HTTP and injectable traits (e.g. the image catalog's `RegistryClient`)
  for things that would otherwise need a daemon.

### Snapshot drift guards

Two committed snapshots are guarded by tests; regenerate them deliberately when a
change is intentional:

- **Supervisor wire protocol** — `treadmill-rs/protocol-schema/*.schema.json`,
  guarded by `treadmill-rs/tests/protocol_schema.rs`. Regenerate:
  `UPDATE_SCHEMA=1 cargo test -p treadmill-rs`.
- **Switchboard OpenAPI** — `switchboard/api-spec/openapi.yaml`, guarded by
  `switchboard/tests/openapi_spec.rs`. Regenerate:
  `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard --test openapi_spec`.

---

## 7. Formatting

Formatting is `treefmt` (`nix/treefmt.nix`): `rustfmt`, `nixfmt`, `statix`,
`deadnix`, `taplo`. SQL, JSON, Markdown, and `.sqlx` are **excluded** (SQL is
formatted separately with `sql-formatter`; the `database` shell has it).

```bash
nix fmt                       # format the whole tree
nix fmt -- path/a.rs path/b.rs   # format only specific files (preferred)
```

**Scope your formatting.** `switchboard/` carries some pre-existing rustfmt
drift; running `nix fmt` with no args will reformat unrelated files. Format only
the files you touched (`nix fmt -- <files>`), or revert unrelated churn before
committing.

---

## 8. Commit & change conventions

- One focused commit per coherent piece. Each commit should build and pass its
  relevant checks on its own.
- Before committing, verify: scoped `cargo build` + `clippy` (changed crates) +
  `nix fmt -- <your files>`, and the relevant hermetic check(s).
- End commit messages with the co-author trailer when applicable, e.g.
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **Bash footgun:** backticks in a `git commit -m "..."` double-quoted message
  get command-substituted by the shell. Use `git commit -F <file>` for messages
  that contain backticks or code.
- Commit or push only when asked; if you're on the default branch, branch first.
- Keep version-bump rationale (CVE/RUSTSEC IDs, etc.) in the commit message, not
  in `Cargo.toml` comments.

---

## 9. Running a supervisor standalone (no switchboard)

To exercise a supervisor on its own — boot one image and watch the console
without the switchboard/console/DB/NATS control plane — use:

```bash
nix run .#qemu-supervisor-local -- --image ghcr.io/<org>/<repo>:<tag>
```

It starts a per-developer `zot` that sources the image from the upstream
registry (on-demand pull-through by default; `--copy` does an upfront `skopeo
copy`), resolves the tag to a manifest digest, and runs the QEMU supervisor
under the switchboard-less **`local` connector** (`connector/local`,
`coord_connector = "local"`). The connector synthesizes one `StartJobMessage`
from CLI flags (`--ssh-key`, `-p key=val`, `--stop-after`, …), streams the guest
console to the terminal, and tears down on guest-exit or Ctrl-C. The wrapper
lives in `tools/local-supervisor.sh` (`--arch`, `--no-kvm`, `--mem`, etc.). For
the full stack instead, use `nix run .#devstack`.

## 10. Pointers

- Architecture & terminology: `README.md`, `doc/img/` (provisional, may be
  outdated pending the post-refactor docs rewrite).
- OCI image migration (active): `doc/oci-image-migration-plan.md`.
- Switchboard protocol refactor: `doc/switchboard-protocol-refactor-plan.md`.
- Schema source of truth: `switchboard/SCHEMA.sql`; migration tooling:
  `switchboard/migrate.sh`.
