{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      self',
      ...
    }:
    let
      cmn = import ./lib.nix { inherit inputs system pkgs; };
      inherit (pkgs) lib;

      switchboardMigrationsSrc = lib.fileset.toSource {
        root = ../switchboard;
        fileset = lib.fileset.unions [
          ../switchboard/SCHEMA.sql
          ../switchboard/migrate.sh
          ../switchboard/migrations
        ];
      };
    in
    {
      checks = {
        clippy = cmn.craneLib.cargoClippy (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-workspace";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
          }
        );

        # Run the workspace test suite via cargo-nextest. Scoped to
        # `--workspace` (NOT `--all-targets`): the per-binary `mkBin`
        # derivations already build `--bins` against their correctly-scoped
        # per-group deps layer, and `--all-targets` here would re-build them
        # against the union-feature `workspaceDeps`, defeating the per-group
        # split. `--no-tests=pass` keeps this green while the workspace has
        # no `#[test]` targets yet; remove it once tests exist and you'd
        # rather a crate accidentally losing all its tests be a CI failure.
        #
        # Tests that need external services (e.g. a Postgres for switchboard)
        # will need wiring here — see `switchboard-migrations-consistency`
        # below for the ephemeral-pg pattern that works inside the Nix
        # sandbox.
        nextest = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-nextest";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoNextestExtraArgs = "--workspace --no-tests=pass";
            partitions = 1;
            partitionType = "count";
          }
        );

        # Run the DB-backed test suite (currently switchboard's
        # `#[sqlx::test]` tests) against an ephemeral Postgres spun up
        # inside the Nix sandbox. These tests are `#[ignore]`d so the
        # default `nextest` check above passes them over without
        # touching a DB; this check is the dedicated place where they
        # actually execute.
        #
        # `--run-ignored only` is the nextest CLI's "only run #[ignore]'d
        # tests" toggle (there is no config-file equivalent in nextest at
        # the time of writing); `--no-tests=pass` keeps the run green
        # for workspace members that have no DB-backed tests at all.
        nextest-db = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-nextest-db";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoNextestExtraArgs = "--workspace --run-ignored only --no-tests=pass";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              pkgs.postgresql
            ];

            preCheck = ''
              PG_BASE_DIR="$(mktemp -d)"
              initdb -D "$PG_BASE_DIR" >/dev/null
              pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" \
                -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

              export PGHOST="$PG_BASE_DIR"
              export PGUSER="$(id -un)"
              createdb -h "$PGHOST" -U "$PGUSER" treadmill_test

              # sqlx::test creates per-test databases from this base
              # connection; the build user is a superuser by default after
              # initdb so CREATE DATABASE works.
              #
              # sqlx rejects URLs with an empty host segment, so the Unix
              # socket directory goes in the host slot URL-encoded
              # (`/foo/bar` -> `%2Ffoo%2Fbar`) rather than via `?host=`.
              ENCODED_PGHOST="$(printf %s "$PGHOST" | sed 's,/,%2F,g')"
              export DATABASE_URL="postgresql://$PGUSER@$ENCODED_PGHOST/treadmill_test"
            '';

            postCheck = ''
              pg_ctl -D "$PG_BASE_DIR" stop >/dev/null 2>&1 || true
            '';
          }
        );

        # TODO: Placeholder for end-to-end integration tests.
        integration-tests = pkgs.runCommand "integration-tests-todo" { } ''
          echo "TODO: end-to-end integration tests"
          mkdir -p $out
        '';

        # Verify that applying switchboard/migrations/ in order reproduces
        # switchboard/SCHEMA.sql exactly (the same check as `./migrate.sh -v`).
        switchboard-migrations-consistency =
          pkgs.runCommand "switchboard-migrations-consistency"
            {
              nativeBuildInputs = with pkgs; [
                postgresql
                atlas
                bash
              ];
            }
            ''
              set -euo pipefail

              cp -r ${switchboardMigrationsSrc} switchboard
              chmod -R u+w switchboard
              cd switchboard

              PG_BASE_DIR="$(mktemp -d)"
              initdb -D "$PG_BASE_DIR" >/dev/null
              pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" \
                -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

              trap 'pg_ctl -D "$PG_BASE_DIR" stop >/dev/null 2>&1 || true' EXIT

              export PGHOST="$PG_BASE_DIR"
              export PGUSER="$(id -un)"

              bash ./migrate.sh -v

              touch $out
            '';
      }
      # Phase 0 of the OCI image migration (doc/oci-image-migration-plan.md
      # §12.2): build the `tiny-efi` fixture and reparse its real wire-format
      # manifest through our `oci-spec`/`parse.rs` view. The `tiny_efi`
      # integration test skips when `TINY_EFI_IMAGE` is unset (so the plain
      # `nextest` check above passes it over); here we point it at the built
      # layout so it does its work. Linux-only: the fixture needs the Linux
      # packing/cross toolchain (see nix/tiny-efi.nix).
      // lib.optionalAttrs pkgs.stdenv.isLinux {
        tiny-efi-image = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-tiny-efi-image";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoNextestExtraArgs = "-p treadmill-rs --no-tests=pass -E 'binary(tiny_efi)'";
            partitions = 1;
            partitionType = "count";

            TINY_EFI_IMAGE = self'.packages.tiny-efi-image-layout;
          }
        );

        # Phase 1 of the OCI image migration (§6/§7/§12.3): drive the
        # `oci_store` client against a real child Zot. The tests spin up Zot
        # (and a second one as a pull-through cache) on loopback and skopeo the
        # `tiny-efi` fixture in, so the check needs zot + skopeo on PATH and the
        # built fixture in TINY_EFI_IMAGE. Like the reparse test, the oci_store
        # tests skip when those are unset, so the plain `nextest` check passes
        # them over. Linux-only (Zot binary + loopback sandbox networking).
        oci-store = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-oci-store";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoNextestExtraArgs = "-p treadmill-supervisor-lib --no-tests=pass -E 'test(oci_store)'";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              cmn.zot
              pkgs.skopeo
            ];

            # oci-client builds a reqwest client (which initializes a TLS
            # backend) even for the plain-HTTP loopback pulls; without a CA
            # bundle in the sandbox that init panics. The connections
            # themselves are HTTP to 127.0.0.1.
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

            TINY_EFI_IMAGE = self'.packages.tiny-efi-image-layout;
          }
        );

        # Phase 2 of the OCI image migration (§6.2/§D9/§12.5): validate the
        # backing-chain emitter against real qemu — assemble the `-blockdev`
        # node graph with qemu-storage-daemon, export it over NBD, and read it
        # back with qemu-io. Needs the qemu tools on PATH; the test skips
        # without them so the plain `nextest` check passes it over.
        chain-assembly = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-chain-assembly";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoNextestExtraArgs = "-p treadmill-supervisor-lib --no-tests=pass -E 'binary(chain_assembly)'";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              pkgs.qemu
            ];
          }
        );
      }
      # Promote each package output to a check so `nix flake check`
      # verifies they all build.
      // self'.packages;
    };
}
