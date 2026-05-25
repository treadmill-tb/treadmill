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
      # Promote each package output to a check so `nix flake check`
      # verifies they all build.
      // self'.packages;
    };
}
