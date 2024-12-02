# shell.nix
{pkgs ? import <nixpkgs> {}}:
with builtins; let
  rust_overlay = import "${pkgs.fetchFromGitHub {
    owner = "nix-community";
    repo = "fenix";
    rev = "1a92c6d75963fd594116913c23041da48ed9e020";
    sha256 = "sha256-L3vZfifHmog7sJvzXk8qiKISkpyltb+GaThqMJ7PU9Y=";
  }}/overlay.nix";

  nixpkgs = import <nixpkgs> {overlays = [rust_overlay];};
  rustBuild = nixpkgs.fenix.fromToolchainFile {file = ./rust-toolchain.toml;};

  pythonPackages = pkgs.python3Packages;

  # Define migra package
  migra = pkgs.python3Packages.buildPythonPackage rec {
    pname = "migra";
    version = "3.0.1663481299"; # Replace with the latest version if necessary

    src = pkgs.python3Packages.fetchPypi {
      inherit pname version;
      sha256 = "sha256-DPDBJdVTAI2f9UAmY6UXA8zEdLtltaT0cnkG2/WOIX8=";
    };
  };
in
  pkgs.mkShell {
    name = "treadmill-dev";
    buildInputs = with pkgs; [
      rustBuild
      openssl
      pkg-config
      postgresql
      sqlx-cli
      migra
      pythonPackages.psycopg2
    ];

    shellHook = ''
      export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
      export OPENSSL_DIR="${pkgs.openssl.dev}"
      export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
      export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"

      # Set up PostgreSQL connection details
      # Please change these according to your environment
      export PGHOST="localhost"
      export PGPORT="5432"
      export PGUSER="ben"
      export PGDATABASE="postgres"
      export DATABASE_URL="postgres://$PGUSER@$PGHOST:$PGPORT/$PGDATABASE"

      echo "DATABASE_URL is set to: $DATABASE_URL"

      echo "Connecting to system PostgreSQL..."

      # Check if PostgreSQL is running
      if ! pg_isready -h $PGHOST -p $PGPORT -U $PGUSER; then
        echo "Error: PostgreSQL is not running. Please start the PostgreSQL service."
        return 1
      fi

      # Create the database if it doesn't exist
      if ! psql -h $PGHOST -p $PGPORT -U $PGUSER -lqt | cut -d \| -f 1 | grep -qw "$PGDATABASE"; then
        createdb -h $PGHOST -p $PGPORT -U $PGUSER "$PGDATABASE"
        echo "Created database $PGDATABASE"
      else
        echo "Database $PGDATABASE already exists"
      fi

      # Execute the schema SQL file (old schema)
      echo "Applying old schema to development database..."
      psql -U $PGUSER -h $PGHOST -d $PGDATABASE -f switchboard/sql/SCHEMA.sql

      # Apply fixtures if needed
      echo "Applying fixtures..."
      psql -U $PGUSER -h $PGHOST -d $PGDATABASE -f switchboard/sql/FIXTURES.sql

      echo "Old schema and fixtures have been applied to the development database."

      # Create temporary databases for Migra comparison
      echo "Creating temporary databases for schema comparison..."

      # Temporary database names
      SOURCE_DB="migra_source_db"
      TARGET_DB="migra_target_db"

      createdb -h $PGHOST -p $PGPORT -U $PGUSER $SOURCE_DB
      createdb -h $PGHOST -p $PGPORT -U $PGUSER $TARGET_DB

      # Load old schema into source_db
      echo "Loading old schema into $SOURCE_DB..."
      psql -U $PGUSER -h $PGHOST -d $SOURCE_DB -f switchboard/sql/SCHEMA.sql

      # Load new schema into target_db
      echo "Loading new schema into $TARGET_DB..."
      psql -U $PGUSER -h $PGHOST -d $TARGET_DB -f switchboard/sql/SCHEMAv2.sql

      # Use Migra to generate migration script
      echo "Generating migration script using Migra..."
      migra --unsafe --schema tml_switchboard postgresql://$PGUSER@$PGHOST:$PGPORT/$SOURCE_DB postgresql://$PGUSER@$PGHOST:$PGPORT/$TARGET_DB > migration.sql

      echo "Migration script generated at migration.sql"

      # Apply migration script to development database
      echo "Applying migration script to development database..."
      psql -U $PGUSER -h $PGHOST -d $PGDATABASE -f migration.sql

      echo "Migration script has been applied to the development database."

      # Clean up temporary databases
      echo "Cleaning up temporary databases..."
      dropdb -h $PGHOST -p $PGPORT -U $PGUSER $SOURCE_DB
      dropdb -h $PGHOST -p $PGPORT -U $PGUSER $TARGET_DB

      echo "Temporary databases have been removed."
    '';
  }
