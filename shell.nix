# Shell expression for the Nix package manager
#
# To use:
#
#  $ nix-shell
#
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
in
  pkgs.mkShell {
    name = "treadmill-dev";
    buildInputs = with pkgs; [
      rustBuild
      openssl
      pkg-config
      postgresql
      sqlx-cli
      go
    ];
    shellHook = ''
      export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
      export OPENSSL_DIR="${pkgs.openssl.dev}"
      export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
      export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"

      # Set up PostgreSQL connection details
      # Please change these
      # export PGHOST=#"localhost"
      # export PGPORT=5432
      # export PGUSER="ben"
      # export PGDATABASE="treadmill_dev"
      export DATABASE_URL="postgres://$PGUSER@$PGHOST:$PGPORT/$PGDATABASE"

      echo $DATABASE_URL

      echo "Connecting to system PostgreSQL..."

      # Check if PostgreSQL is running
      if ! pg_isready -h $PGHOST -p $PGPORT -U $PGUSER; then
        echo "Error: PostgreSQL is not running. Please start the PostgreSQL service."
        return 1
      fi

      # Create the database if it doesn't exist
      if ! psql -lqt | cut -d \| -f 1 | grep -qw "$PGDATABASE"; then
        createdb "$PGDATABASE"
        echo "Created database $PGDATABASE"
      else
        echo "Database $PGDATABASE already exists"
      fi

      # Execute the schema SQL file
      echo "Applying schema..."
      psql -U $PGUSER -h $PGHOST -d $PGDATABASE -f switchboard/sql/SCHEMAv2.sql

      # Execute the fixtures SQL file
      echo "Applying fixtures..."
      psql -U $PGUSER -h $PGHOST -d $PGDATABASE -f switchboard/sql/FIXTURESv2.sql

      echo "Schema and fixtures have been applied to the database."

      # Install pg-schema-diff
      go install github.com/stripe/pg-schema-diff/cmd/pg-schema-diff@latest

      # Add Go binaries to PATH
      export PATH="$HOME/go/bin:$PATH"
    '';
  }
