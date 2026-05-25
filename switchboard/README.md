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

## TODO: Outdated Development Guide

We use a Nix shell environment to provide all dependencies required
for working on the Treadmill Switchboard. Enter the Nix-shell in the
project's root to bring all required dependencies into your PATH:
```
[you@computer:~/treadmill]$ nix-shell # This might take a while
[nix-shell:~/treadmill]$ # Now you have rustup, httpie, etc. available
```

To spin up a development server, you need to run an instance of
Postgres. We provide another Nix shell environment to run an ephemeral
Postgres instance. For this, change into the `switchboard/` directory,
and run the following command:
```
[nix-shell:~/treadmill]$ cd switchboard/
[nix-shell:~/treadmill/switchboard]$ nix-shell database-shell.nix

...

waiting for server to start.... done
server started

[nix-shell:~/treadmill/switchboard]$ # Now you can access the Postgres database with `psql`:
[nix-shell:~/treadmill/switchboard]$ psql
psql (17.5)
Type "help" for help.

postgres=#
```

Before running a development server, we can load the database schema
and test fixtures:
```
[nix-shell:~/treadmill/switchboard]$ psql < SCHEMA.sql
CREATE SCHEMA
CREATE TYPE
[...]
CREATE TABLE
CREATE TABLE

[nix-shell:~/treadmill/switchboard]$ psql < FIXTURES.sql
TRUNCATE TABLE
NOTICE:  truncate cascades to table "api_tokens"
[...]
INSERT 0 1
INSERT 0 5
```

Now, in this database shell environment, start a development server:
```
[nix-shell:~/treadmill/switchboard]$ cargo run --bin swx -- serve -c config.example.toml
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.26s
     Running `$HOME/treadmill/target/debug/swx serve -c config.example.toml`
```

You can then use the `req.sh` utility to connect to the Switchboard
API using `httpie`. This script automatically sets the `Authorization`
header to a token valid for the dummy admin user included in the
`FIXTURES.sql` file:
```
[nix-shell:~/treadmill/switchboard]$ ./req.sh GET api/v1/jobs
GET /api/v1/jobs HTTP/1.1
Accept: */*
Accept-Encoding: gzip, deflate
Authorization: Bearer B1oy2ko1wV...
Connection: keep-alive
Host: localhost:8080
User-Agent: HTTPie/3.2.4


HTTP/1.1 200 OK
content-length: 23
content-type: application/json
date: Sat, 19 Jul 2025 20:02:10 GMT

{
    "jobs": {},
    "type": "ok"
}
```
