# `tml-switchboard`

## Switchboard Database Migrations

The Switchboard's PostgreSQL schema is maintained in two sets of files that must
agree at all times:

- **`switchboard/SCHEMA.sql`** — the hand-written, commented source of truth
  describing the desired schema. When working on the switchboard, edits to the
  schema should primarily go into this file.

- **`switchboard/migrations/`** — an ordered sequence of timestamped migration
  files. These are embedded into the Switchboard binary by `build.rs` and
  applied automatically at startup via `sqlx::migrate!()`.

[Atlas](https://atlasgo.io) is used to keep the two in sync: it generates new
migrations by diffing the cumulative state of `migrations/` against
`SCHEMA.sql`, and it verifies in CI that replaying `migrations/` reproduces
`SCHEMA.sql` exactly. The `migrations/atlas.sum` file is Atlas's integrity
manifest and is committed alongside the migration files; sqlx ignores it at
runtime.

### Working on migrations

Enter the database devshell using `nix develop '.#database'`, which provisions
an ephemeral PostgreSQL instance and exports `PGHOST`/`PGUSER` for the
`migrate.sh` helper script.

Then, from the `switchboard/` directory:

| Command                  | What it does                                                                                                                              |
|--------------------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| `./migrate.sh -c <name>` | Generate `migrations/<timestamp>_<name>.sql` from the diff between the current `migrations/` state and `SCHEMA.sql`. Updates `atlas.sum`. |
| `./migrate.sh -v`        | Verify that applying all migrations in order reproduces `SCHEMA.sql`. Exits non-zero on drift; also run as a `nix flake check`.           |
| `./migrate.sh -r`        | Re-hash `migrations/atlas.sum` after manually editing a migration file (e.g., to add a backfill step that Atlas could not infer).         |
