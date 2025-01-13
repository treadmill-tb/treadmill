{pkgs ? import <nixpkgs> {}}:

with builtins;

let
  # Override pythonPackages to use Flask 2.2.5
  pythonPackages = pkgs.python3Packages.override {
    overrides = self: super: {
      flask = self.buildPythonPackage rec {
        pname = "Flask";
        version = "2.2.5";
        src = self.fetchPypi {
          inherit pname version;
          sha256 = "sha256-7e6bCn/yZiG9WowQ/0hK4oc3okENmbC7mmhQx/uXeqA=";
        };
        propagatedBuildInputs = with self; [
          itsdangerous
          click
          jinja2
          werkzeug
        ];
      };

      sqlbag = self.buildPythonPackage rec {
        pname = "sqlbag";
        version = "0.1.1617247075";
        src = self.fetchPypi {
          inherit pname version;
          sha256 = "sha256-udeGLDsgMDVteWyocpB5Yv1UcEBml4166JOD9RIzZu0=";
        };
        propagatedBuildInputs = with self; [
          flask
          sqlalchemy
          psycopg2
          six
        ];
        doCheck = false;
      };
    };
  };

  # Define migra package
  migra = pythonPackages.buildPythonPackage rec {
    pname = "migra";
    version = "3.0.1663481299";
    src = pythonPackages.fetchPypi {
      inherit pname version;
      sha256 = "sha256-DPDBJdVTAI2f9UAmY6UXA8zEdLtltaT0cnkG2/WOIX8=";
    };
    propagatedBuildInputs = with pythonPackages; [
      schemainspect
      sqlbag
      psycopg2
      click
    ];
  };

  schemainspect = pythonPackages.buildPythonPackage rec {
    pname = "schemainspect";
    version = "3.1.1663587362";
    src = pythonPackages.fetchPypi {
      inherit pname version;
      sha256 = "sha256-opWtVvehnAnl4e+fFtrb9jkuJhlstfBbWv5hPJnOdGg=";
    };
    propagatedBuildInputs = with pythonPackages; [
      sqlbag
      setuptools
    ];
  };
in
  pkgs.mkShell {
    name = "treadmill-db-migrate-shell";

    buildInputs = with pkgs; [
      postgresql
      migra
      schemainspect
      pythonPackages.psycopg2
      sql-formatter
      sqlx-cli
    ];

    shellHook = ''
      # Create a temporary directory in which we can initialize a new
      # Postgres database:
      export PG_BASE_DIR="$(mktemp -d -t tmlswbmigratedb-XXXXX)"

      echo "Initializing new Postgres database in $PG_BASE_DIR"
      initdb -D "$PG_BASE_DIR"

      # Start PostgreSQL running as the current user and with the Unix
      # socket in its databse dir. Passing an empty '-h' parameter
      # avoids opening a TCP socket which could conflict with another
      # (system-wide) Postgres instance.
      pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

      # Cleanup function to be called on shell exit
      function cleanup() {
          # Try to shut down the database
          pg_ctl -D "$PG_BASE_DIR" stop

          # Remove the database directory
          echo "Deleting database state $PG_BASE_DIR"
          rm -rf "$PG_BASE_DIR"
      }
      trap cleanup EXIT

      # Export the PostgreSQL connection details
      export PGHOST="$PG_BASE_DIR"
      export PGHOST_URIENCODE="$(printf '%s' "$PGHOST" | ${pkgs.jq}/bin/jq -sRr @uri)"
      export PGUSER="$(whoami)"
      export PGDATABASE="postgres"
      export DATABASE_URL="postgresql://''${PGUSER}@''${PGHOST_URIENCODE}/''${PGDATABASE}"

      # Set the config parameters for running the Switchboard on this ephemeral DB:
      export TML_DATABASE__HOST="$PGHOST"
      export TML_DATABASE__DATABASE="$PGDATABASE"
      export TML_DATABASE__USER="$PGUSER"
    '';
  }
