# AGENTS.md — Treadmill Development Guide

This file documents how the Treadmill repository is structured and how to build,
test, and change it.

## 1. What Is Treadmill

Treadmill is a **distributed hardware testbed**: it uses a central "switchboard"
to schedule jobs onto real or virtual hardware "hosts" and gives users an
image-based, reproducible way to run workloads on them.

| Component                             | Crate(s)           | Role                                          |
|---------------------------------------|--------------------|-----------------------------------------------|
| **Switchboard** (central coordinator) | `switchboard/`     | Central coordinator.                          |
| **Supervisors**                       | `supervisor/*`     | Control and manage hosts.                     |
| **Connectors**                        | `connector/*`,     | Protocol between switchboard and supervisors. |
| **Puppet**                            | `puppet/`          | Agent on hosts talking to supervisors.        |
| **Control Sockets**                   | `control-socket/*` | Protocol between supervisors and puppet.      |
| **CLI**                               | `cli/`             | `tml` user-facing command-line client.        |
| **Shared Library**                    | `treadmill-rs/`    | Common types & infrastructure.                |
| **Web Console (SPA)**                 | `console/`         | Browser frontend for the switchboard API.     |

## 2. Toolchain & Development Environment (Nix)

**Development is driven through the Nix flake.** The pinned Rust toolchain and
all external tools are available through Nix dev shells, they cannot be assumed
to be on the bare `PATH`. Always run commands inside a dev shell, e.g.:

```bash
nix develop --command bash -c 'cargo build -p treadmill-rs' # default shell
```

### Nix Dev Shells (`nix/devshells.nix`)

- **`default`** -- default shell. Has the Rust toolchain (`cargo`, `clippy`,
  `rustfmt`), `nixfmt`, plus the heavier externals used by tests: `zot`,
  `skopeo`, `qemu`, the AAVMF firmware env vars (`TML_AAVMF_*`) for the boot
  test, and the NATS log-streaming tools (`nats-server`, `nsc`, `nats`).
  `SQLX_OFFLINE=1` (no DB available), so builds use the committed `.sqlx` query
  cache.

- **`database`** -- adds `postgresql`, `atlas`, `sqlx-cli`, `sql-formatter`. On
  entry it **spins up an ephemeral Postgres** on a unix socket and exports
  variables like `PGHOST`, `PGUSER`, `PGDATABASE=postgres`, and `DATABASE_URL`.
  Use this shell for anything that needs a live database: running
  `#[sqlx::test]` tests, regenerating `.sqlx`, or authoring migrations. The
  cluster is torn down when the shell's process exits. `SQLX_OFFLINE=1` is still
  set by default, but `cargo sqlx prepare` commands ignore that setting.

- **`images`** -- the libguestfs image-build pipeline tooling
  (`virt-customize`/`guestfish` with bundled appliance, `mtools`, `qemu-img`,
  `xz`, `curl`; `LIBGUESTFS_BACKEND=direct`). Standalone (no Rust/PG/NATS): the
  in-progress `images/lib/build-image.sh` is plain shell that consumes the
  Nix-built `tml-puppet` / `image-util` binaries. Image builds need privileged
  libguestfs + network (live apt), so they run **in this shell**, not as a
  hermetic Nix check.

  TODO: revisit this, this shell might be stale now that image builds run
  without libguestfs on GH actions.

### Nix Dev Apps

The Nix flake provides multiple convenience dev apps:

- `nix run '.#switchboard-sqlx-prepare'`: regenerates the `.sqlx` query cache,
  ensuring that all migrations have been applied and the `cargo sqlx prepare` is
  run with the correct options, in the correct subdirectory.

- `nix run '.#switchboard-migrate'`: runs `switchboard/migrate.sh` in a database
  devshell environment with an ephemeral PostgreSQL instance.

  switchboard/SCHEMA.sql is the hand-written source of truth. The migrations/
  directory holds the ordered, derived migration files (also applied at runtime
  by sqlx::migrate!() in build.rs). This script keeps the two in sync.

  Run as: nix run '.#switchboard-migrate' $OPTS

    -c <name>   author a new migration from the current diff between cumulative
                migrations/ and SCHEMA.sql
    -v          verify migrations/ reproduces SCHEMA.sql
    -r          re-hash migrations/atlas.sum after a manual edit

  As we're using the community version of Atlas, certain things like functions
  and triggers may not be migrated and must be manually added.

  The `switchboard-migrations-consistency` flake check enforces that the SCHEMA
  and migrations are consistent.

### Web console (`console/`)

The SPA console is an npm package (React + React Router v7 in SPA mode,
`ssr: false`), not a Cargo crate. Its API client types are **generated** from
`switchboard/api-spec/openapi.yaml` into the committed
`console/app/api/schema.d.ts`; after any switchboard API change, regenerate
and commit the diff:

```bash
cd console && npm ci && npm run codegen
```

The `console` Nix package (`nix build .#console`, auto-promoted to a
flake check) is the frontend CI gate: it fails on schema drift, then runs
`npm run lint`, `npm run typecheck` (strict tsc), and the vite build. Dev loop:
`npm run dev` (in the default dev shell, which carries node) proxies `/api` to
a local switchboard at `127.0.0.1:8081` (override with `TML_DEV_PROXY`). The
build reads `VITE_TML_API_URL` for the switchboard origin; empty means
same-origin. `doc/console-neo-plan.md` tracks the remaining build-out.

## 3. Building, Testing, and the Nix Checks

Build / lint / test a specific crate (fast inner loop):

```bash
nix develop --command bash -c 'cargo build -p treadmill-switchboard'
nix develop --command bash -c 'cargo clippy -p treadmill-switchboard --all-targets'
nix develop --command bash -c 'cargo test  -p treadmill-rs'
```

The authoritative gates are the **flake checks** (`nix/checks.nix`), run
hermetically. Always ensure all default flake checks pass by running `nix flake
check` on every commit, except where that doesn't make sense (deliberate
intermediate commit with breakage). The checks can be accelerated using the
Cachix cache (requires to be a trusted Nix user to accept the flake config),
which holds a pre-built crane dependency layer.

## 4. Testing conventions

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
  external HTTP and injectable traits (e.g. the image catalog's
  `RegistryClient`) for things that would otherwise need a daemon.

### Snapshot drift guards

Two committed snapshots are guarded by tests; regenerate them deliberately when
a change is intentional:

- **Supervisor wire protocol** — `treadmill-rs/protocol-schema/*.schema.json`,
  guarded by `treadmill-rs/tests/protocol_schema.rs`. Regenerate:
  `UPDATE_SCHEMA=1 cargo test -p treadmill-rs`.
- **Switchboard OpenAPI** — `switchboard/api-spec/openapi.yaml`, guarded by
  `switchboard/tests/openapi_spec.rs`. Regenerate:
  `UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard --test openapi_spec`.

## 5. Formatting

Formatting is `treefmt` (`nix/treefmt.nix`).

```bash
nix fmt                          # format the whole tree
nix fmt -- path/a.rs path/b.rs   # format only specific files (preferred)
```

Each commit must be properly formatted, the Nix flake checks also include a
formatting check.

## 6. Commit & change conventions

- One focused commit per coherent piece. Each commit should build and pass the
  flake checks on its own (apart from documented exceptions).
- End commit messages with the co-author trailer when applicable, e.g.
  `Co-Authored-By: A Coding Agent <some.email@example.org>`.
- For coding agents: backticks in a `git commit -m "..."` double-quoted message
  get command-substituted by the shell. Use `git commit -F <file>` for messages
  that contain backticks or code.
- Commit or push only when asked; if you're on the default branch, branch first.
- Keep version-bump rationale (CVE/RUSTSEC IDs, etc.) in the commit message, not
  in `Cargo.toml` comments.

## 7. Running a supervisor standalone (no switchboard)

To exercise a supervisor on its own (boot one image and watch the console
without the switchboard/console/DB/NATS control plane) use:

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
