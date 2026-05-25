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

      defaultShell = cmn.craneLib.devShell {
        packages = with pkgs; [
          pkg-config
          openssl
          sqlx-cli
          nixfmt
          statix
          deadnix
          taplo
          cargo-audit
          cargo-outdated
        ];

        shellHook = ''
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:''${PKG_CONFIG_PATH:-}"
          export OPENSSL_DIR="${pkgs.openssl.dev}"
          export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
          export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"
          export LD_LIBRARY_PATH="${pkgs.openssl.out}/lib:''${LD_LIBRARY_PATH:-}"
        '';
      };

      databaseShell = pkgs.mkShell {
        name = "treadmill-db-migrate-shell";

        packages = with pkgs; [
          postgresql
          atlas
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
        database = databaseShell;
      };
    };
}
