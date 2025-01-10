#! /usr/bin/env nix-shell
#! nix-shell -i bash database-shell.nix

# The above expression ensures that we're running in a Nix shell with an
# emphemeral database instance.

set -e

function usage() {
    echo "Usage: $(basename $0) [-c <migration name>] [-v]" >&2
}

function create_migration_dbs() {
    # Create a set of temporary databases for migra to be able to diff:
    SOURCE_DB="tml_switchboard_migra_source_db"
    createdb -h $PGHOST -U $PGUSER $SOURCE_DB
    SOURCE_DATABASE_URL_URIENCODE="postgresql://${PGUSER}@${PGHOST_URIENCODE}/${SOURCE_DB}"
    SOURCE_DATABASE_URL_HOSTPARAM="postgresql://${PGUSER}@/${SOURCE_DB}?host=${PGHOST}"

    TARGET_DB="tml_switchboard_migra_target_db"
    createdb -h $PGHOST -U $PGUSER $TARGET_DB
    TARGET_DATABASE_URL_URIENCODE="postgresql://${PGUSER}@${PGHOST_URIENCODE}/${TARGET_DB}"
    TARGET_DATABASE_URL_HOSTPARAM="postgresql://${PGUSER}@/${TARGET_DB}?host=${PGHOST}"
}

function op_create_migration() {
    create_migration_dbs

    # Apply all existing migrations to the source database:
    echo "Applying existing migrations to the source database." >&2
    DATABASE_URL="$SOURCE_DATABASE_URL_URIENCODE" sqlx migrate run

    # Apply the target schema to the destination database:
    echo "Loading new schema into the target database." >&2
    psql -v ON_ERROR_STOP=1 -U $PGUSER -h $PGHOST -d $TARGET_DB -f ./SCHEMA.sql

    # Create a new SQLX migration file:
    echo "Creating new SQLx migration file..."
    MIGRATION_SQL="$(sqlx migrate add -t -- "$1" | cut -d' ' -f 2-)"

    # Use Migra to generate migration script
    echo "Generating migration script using Migra..." >&2
    migra \
        --unsafe \
        --schema tml_switchboard \
        "$SOURCE_DATABASE_URL_HOSTPARAM" \
        "$TARGET_DATABASE_URL_HOSTPARAM" \
        > "$MIGRATION_SQL" || true
    echo "Script generated: $MIGRATION_SQL"
}

function op_validate() {
    create_migration_dbs

    # Apply all existing migrations to the source database:
    echo "Applying existing migrations to the source database." >&2
    DATABASE_URL="$SOURCE_DATABASE_URL_URIENCODE" sqlx migrate run

    # Apply the target schema to the destination database:
    echo "Loading new schema into the target database." >&2
    psql -v ON_ERROR_STOP=1 -U $PGUSER -h $PGHOST -d $TARGET_DB -f ./SCHEMA.sql

    # Ensure that the diff between the two databases is empty:
    TEMP_SQL_DIFF="$(mktemp -t tmlswbdbdiff-XXXXX.sql)"
    echo "Generating diff using Migra (at $TEMP_SQL_DIFF)..." >&2
    migra \
        --unsafe \
        --schema tml_switchboard \
        "$SOURCE_DATABASE_URL_HOSTPARAM" \
        "$TARGET_DATABASE_URL_HOSTPARAM" \
        > "$TEMP_SQL_DIFF" || true

    if [ -s "$TEMP_SQL_DIFF" ]; then
	echo "Database schema diff at $TEMP_SQL_DIFF is non-empty! Validation failed. Diff:" >&2
	echo "" >&2
	cat "$TEMP_SQL_DIFF"
	exit 1
    else
	echo "Database schema diff is empty, validation succeeded." >&2
	rm "$TEMP_SQL_DIFF"
	exit 0
    fi
}

OPERATION=""

while getopts ':c:vh' opt; do
    case "$opt" in
	c)
	    test "$OPERATION" == "" \
		|| (echo "Create migration (-c) conflicts with operation $OPERATION" >&2 || exit 1)
	    OPERATION="create-migration"
	    MIGRATION_LABEL="$OPTARG"
	    ;;

	v)
	    test "$OPERATION" == "" \
		|| (echo "Validate (-v) conflicts with operation $OPERATION" >&2 || exit 1)
	    OPERATION="validate"
	    ;;

	h)
	    usage
	    exit 0
	    ;;

	:)
	    echo "option requires an argument." >&2
	    usage
	    exit 1
	    ;;

	?)
	    echo "Invalid command option." >&2
	    usage
	    exit 1
	    ;;
    esac
done
shift "$(($OPTIND -1))"

if [ "$OPERATION" == "create-migration" ]; then
    op_create_migration "$MIGRATION_LABEL"
elif [ "$OPERATION" == "validate" ]; then
    op_validate
else
    echo "Missing operation, pass either -c or -v." >&2
    usage
    exit 1
fi
