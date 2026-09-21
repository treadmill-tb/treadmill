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
          postgresql
          sql-formatter

          # OCI image-migration tooling: the vendored Zot registry (per-server
          # store daemon) plus skopeo for moving images between OCI layouts and
          # registries in tests and the CLI.
          cmn.zot
          skopeo

          # qemu provides qemu-img (backing-chain validation) and qemu-system-*
          # for boot test
          qemu

          # Log streaming: the NATS server + JetStream (`nats run .#dev` runs a
          # live broker), `nsc` to bootstrap the decentralized-JWT auth
          # hierarchy, and the `nats` CLI for ad-hoc pub/sub against it. Note:
          # nats-server binds a TCP/WebSocket port, so it cannot run in the
          # restricted sandbox (see AGENTS.md §2) — verify NATS-touching code via
          # its hermetic Nix check, not by running the daemon by hand. `nsc`
          # itself only writes files and runs fine in the sandbox.
          nats-server
          nsc
          natscli

          # Web console (console/): npm-driven Vite/React toolchain.
          nodejs_22

          # FAT tooling for the nbd-netboot daemon tests.
          dosfstools
          mtools

          # The nbd-netboot supervisor's TFTP server.
          cmn.nbdfatftpd
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
            atlas
            sqlx-cli
          ]);

        # Bring up the throwaway Postgres cluster + DATABASE_URL via the shared
        # snippet (also used by the `switchboard-sqlx-prepare` app).
        shellHook =
          defaultShell.shellHook
          + cmn.ephemeralPostgresHook
          + ''
            # sqlx gives each #[sqlx::test] its own pool, but all of them draw
            # from one 20-connection master pool shared by the whole test
            # process. libtest's default of one thread per core overruns that,
            # and tests then fail waiting for a connection. `cargo nextest run`
            # is unaffected: it forks a process per test, so each test gets its
            # own master pool.
            export RUST_TEST_THREADS="''${RUST_TEST_THREADS:-8}"
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
