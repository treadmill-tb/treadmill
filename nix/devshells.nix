{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      ...
    }:
    let
      cmn = import ./lib.nix { inherit inputs system pkgs; };
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;

      defaultShell = cmn.craneLib.devShell {
        packages = with pkgs; [
          pkg-config
          openssl
          sqlx-cli
          nixfmt
          statix
          deadnix
          taplo
        ];

        shellHook = ''
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:''${PKG_CONFIG_PATH:-}"
          export OPENSSL_DIR="${pkgs.openssl.dev}"
          export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
          export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"
          export LD_LIBRARY_PATH="${pkgs.openssl.out}/lib:''${LD_LIBRARY_PATH:-}"
        '';
      };

      # Ephemeral PostgreSQL devshell for switchboard / sqlx work:
      pythonPackages = pkgs.python3Packages.override {
        overrides = self: _super: {
          flask = self.buildPythonPackage rec {
            pname = "Flask";
            version = "2.2.5";
            src = self.fetchPypi {
              inherit pname version;
              sha256 = "sha256-7e6bCn/yZiG9WowQ/0hK4oc3okENmbC7mmhQx/uXeqA=";
            };
            pyproject = true;
            build-system = [ pythonPackages.setuptools ];
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
            pyproject = true;
            build-system = [ pythonPackages.setuptools ];
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

      schemainspect = pythonPackages.buildPythonPackage rec {
        pname = "schemainspect";
        version = "3.1.1663587362";
        src = pythonPackages.fetchPypi {
          inherit pname version;
          sha256 = "sha256-opWtVvehnAnl4e+fFtrb9jkuJhlstfBbWv5hPJnOdGg=";
        };
        pyproject = true;
        build-system = [ pythonPackages.setuptools ];
        propagatedBuildInputs = with pythonPackages; [
          sqlbag
          setuptools
        ];
      };

      migra = pythonPackages.buildPythonPackage rec {
        pname = "migra";
        version = "3.0.1663481299";
        src = pythonPackages.fetchPypi {
          inherit pname version;
          sha256 = "sha256-DPDBJdVTAI2f9UAmY6UXA8zEdLtltaT0cnkG2/WOIX8=";
        };
        pyproject = true;
        build-system = [ pythonPackages.setuptools ];
        propagatedBuildInputs = with pythonPackages; [
          schemainspect
          sqlbag
          psycopg2
          click
        ];
      };

      databaseShell = pkgs.mkShell {
        name = "treadmill-db-migrate-shell";

        packages = with pkgs; [
          postgresql
          migra
          schemainspect
          pythonPackages.psycopg2
          sql-formatter
          sqlx-cli
        ];

        shellHook = ''
          export PG_BASE_DIR="$(mktemp -d -t tmlswbmigratedb-XXXXX)"

          echo "Initializing new Postgres database in $PG_BASE_DIR"
          initdb -D "$PG_BASE_DIR"

          pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" \
            -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

          cleanup() {
            pg_ctl -D "$PG_BASE_DIR" stop
            echo "Deleting database state $PG_BASE_DIR"
            rm -rf "$PG_BASE_DIR"
          }
          trap cleanup EXIT

          export PGHOST="$PG_BASE_DIR"
          export PGHOST_URIENCODE="$(printf '%s' "$PGHOST" | ${pkgs.jq}/bin/jq -sRr @uri)"
          export PGUSER="$(whoami)"
          export PGDATABASE="postgres"
          export DATABASE_URL="postgresql://''${PGUSER}@''${PGHOST_URIENCODE}/''${PGDATABASE}"

          export TML_DATABASE__HOST="$PGHOST"
          export TML_DATABASE__DATABASE="$PGDATABASE"
          export TML_DATABASE__USER="$PGUSER"
        '';
      };
    in
    {
      devShells = {
        default = defaultShell;
      }
      // lib.optionalAttrs isLinux { database = databaseShell; };
    };
}
