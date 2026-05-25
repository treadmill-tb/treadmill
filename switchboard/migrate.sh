#! /usr/bin/env bash
#
# Manage switchboard SQL migrations using Atlas.
#
# SCHEMA.sql is the hand-written source of truth. The migrations/ directory
# holds the ordered, derived migration files (also applied at runtime by
# sqlx::migrate!() in build.rs). Atlas keeps the two in sync:
#
#   ./migrate.sh -c <name>   author a new migration from the current diff
#                            between cumulative migrations/ and SCHEMA.sql
#   ./migrate.sh -v          verify migrations/ reproduces SCHEMA.sql
#   ./migrate.sh -r          re-hash migrations/atlas.sum after a manual edit
#
# Run inside the .#database devshell, which provides Postgres + Atlas and an
# ephemeral PGHOST/PGUSER.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

: "${PGHOST:?Run inside the .#database devshell}"
: "${PGUSER:?Run inside the .#database devshell}"

DEV_DB="tml_switchboard_atlas_dev"
DEV_URL="postgres://${PGUSER}@/${DEV_DB}?host=${PGHOST}&sslmode=disable"
DIR="file://migrations"
SCHEMA_FILE="file://SCHEMA.sql"
SCOPE=(--schema tml_switchboard)

# Atlas reads schema state by replaying SQL into a dev database. Postgres
# auto-creates a `public` schema in any new DB, which atlas would otherwise
# diff against SCHEMA.sql and want to drop. Start each run from a DB with
# only the schemas SCHEMA.sql defines.
reset_dev_db() {
    dropdb -h "$PGHOST" -U "$PGUSER" --if-exists "$DEV_DB" >/dev/null
    createdb -h "$PGHOST" -U "$PGUSER" "$DEV_DB"
    psql -h "$PGHOST" -U "$PGUSER" -d "$DEV_DB" -q \
        -c 'DROP SCHEMA IF EXISTS public CASCADE' >/dev/null
}
trap 'dropdb -h "$PGHOST" -U "$PGUSER" --if-exists "$DEV_DB" >/dev/null 2>&1 || true' EXIT

usage() {
    sed -n '3,16p' "$0" >&2
}

OPERATION=""
NAME=""

while getopts ':c:vrh' opt; do
    case "$opt" in
        c) OPERATION="create"; NAME="$OPTARG" ;;
        v) OPERATION="validate" ;;
        r) OPERATION="rehash" ;;
        h) usage; exit 0 ;;
        :) echo "Option -$OPTARG requires an argument." >&2; usage; exit 1 ;;
        \?) echo "Unknown option: -$OPTARG" >&2; usage; exit 1 ;;
    esac
done

case "$OPERATION" in
    create)
        reset_dev_db
        atlas migrate diff "$NAME" \
            --dir "$DIR" --to "$SCHEMA_FILE" --dev-url "$DEV_URL" "${SCOPE[@]}"
        ;;
    validate)
        atlas migrate validate --dir "$DIR"
        reset_dev_db
        diff_output=$(atlas schema diff \
            --from "$DIR" --to "$SCHEMA_FILE" --dev-url "$DEV_URL" "${SCOPE[@]}")
        if [ "$diff_output" != "Schemas are synced, no changes to be made." ]; then
            echo "Schema drift between migrations/ and SCHEMA.sql:" >&2
            echo "$diff_output" >&2
            exit 1
        fi
        echo "Migrations are in sync with SCHEMA.sql." >&2
        ;;
    rehash)
        atlas migrate hash --dir "$DIR"
        ;;
    *)
        usage
        exit 1
        ;;
esac
