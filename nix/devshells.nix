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
          cargo-nextest
          cargo-outdated

          # OCI image-migration tooling: the vendored Zot registry (per-server
          # store daemon / pull-through cache) plus skopeo for moving images
          # between OCI layouts and registries in tests and the CLI.
          cmn.zot
          skopeo
        ];

        shellHook = ''
          # Check sqlx query macros against the committed `.sqlx` cache, matching
          # CI (nix/lib.nix). Without this the `database` shell exports a
          # DATABASE_URL for its empty ephemeral Postgres, so the macros try to
          # compile against an unmigrated DB and fail. `cargo sqlx prepare`
          # overrides this back to false internally, so the cache can still be
          # regenerated from here.
          export SQLX_OFFLINE="true"

          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:''${PKG_CONFIG_PATH:-}"
          export OPENSSL_DIR="${pkgs.openssl.dev}"
          export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
          export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"
          export LD_LIBRARY_PATH="${pkgs.openssl.out}/lib:''${LD_LIBRARY_PATH:-}"
        '';
      };

      databaseShell = pkgs.mkShell {
        name = "treadmill-db-migrate-shell";

        # `mkShell { packages = [ ...]; }` gets turned into `nativeBuildInputs`:
        packages =
          defaultShell.nativeBuildInputs
          ++ (with pkgs; [
            postgresql
            atlas
            sql-formatter
            sqlx-cli
          ]);

        shellHook = defaultShell.shellHook + ''
          export PG_BASE_DIR="$(mktemp -d /tmp/tmlswbmigratedb-XXXXX)"

          echo "Initializing new Postgres database in $PG_BASE_DIR"
          initdb -D "$PG_BASE_DIR"

          pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" \
            -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

          # Tear down Postgres and its scratch dir once the shell is gone. A
          # plain `trap ... EXIT` is not enough: `nix develop -c CMD` execs the
          # command, replacing the shell that holds the trap, so it never fires
          # and the detached pg_ctl daemon is orphaned in /tmp. Instead, fork a
          # watcher keyed on this shell's PID. exec preserves the PID, so the
          # watcher fires on every exit path -- interactive exit, `-c`, even a
          # killed terminal -- and only ever touches its own cluster, so
          # concurrent database shells don't clean up each other.
          __tml_db_pid=$$
          (
            while kill -0 "$__tml_db_pid" 2>/dev/null; do sleep 1; done
            pg_ctl -D "$PG_BASE_DIR" stop -m immediate >/dev/null 2>&1
            rm -rf "$PG_BASE_DIR"
          ) &
          disown

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
