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
        shellcheck =
          let
            shellScripts = pkgs.lib.fileset.fileFilter (file: file.hasExt "sh") ../.;
            shellcheckSrc = pkgs.lib.fileset.toSource {
              root = ../.;
              fileset = shellScripts;
            };
          in
          pkgs.runCommand "treadmill-shellcheck" { } ''
            pushd "${shellcheckSrc}"
            for SCRIPT in $(find . -type f); do
              echo "Checking $SCRIPT" >&2
              ${pkgs.shellcheck}/bin/shellcheck "$SCRIPT" || exit 1
            done
            echo "All scripts pass shellcheck!" >&2
            touch $out
            popd
          '';

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
        # `--workspace` (NOT `--all-targets`): the binaries are built
        # separately by `mkBin` against the shared `binDeps` layer, so there's
        # no need to rebuild them here against the dev-dep-carrying
        # `workspaceDeps`. `--no-tests=pass` keeps this green while the
        # workspace has no `#[test]` targets yet; remove it once tests exist
        # and you'd rather a crate accidentally losing all its tests be a CI
        # failure.
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
            cargoArtifacts = cmn.testArtifacts;
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
            cargoArtifacts = cmn.testArtifacts;
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
            cargoArtifacts = cmn.testArtifacts;
            cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'binary(tiny_efi)'";
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
            cargoArtifacts = cmn.testArtifacts;
            # The leases-as-references tests also live in `oci_store::tests` but
            # drive GC and take tens of seconds each; they have their own `lease`
            # check below, so exclude them here.
            cargoNextestExtraArgs =
              "--workspace --no-tests=pass "
              + "-E 'test(oci_store) & !test(lease_pins_against_gc) & !test(parallel_ensure_present_while_pinned)'";
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

        # Phase 3 of the OCI image migration (§7.3/§7.4/§12.3): prove the
        # leases-as-references model against the real Zot binary. The tests pin
        # an `inuse-<job>` reference, drive Zot's GC, and assert the pinned
        # closure is retained while an unreferenced image is collected (and that
        # releasing the lease makes the closure collectible). Same external needs
        # as `oci-store` (zot + skopeo + the fixture); the tests skip when unset
        # so the plain `nextest` check passes them over. Linux-only.
        lease = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-lease";
            version = "0.1.0";
            cargoArtifacts = cmn.testArtifacts;
            cargoNextestExtraArgs =
              "--workspace --no-tests=pass "
              + "-E 'test(lease_pins_against_gc) | test(parallel_ensure_present_while_pinned)'";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              cmn.zot
              pkgs.skopeo
            ];

            # See the oci-store check: oci-client's reqwest TLS init needs a CA
            # bundle present even though the loopback traffic is plain HTTP.
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
            cargoArtifacts = cmn.testArtifacts;
            cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'binary(chain_assembly)'";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              pkgs.qemu
            ];
          }
        );

        # Phase 2 deliverable (§12.6): the aarch64 boot test. It pushes the
        # tiny-efi fixture into a child Zot, points the qemu supervisor's
        # OciStore at it, and drives the real job core under non-accelerated
        # (TCG) qemu-system-aarch64 -M virt + AAVMF, asserting the guest prints
        # the overlay sentinel (and never the base-only tripwire). Needs zot +
        # skopeo + qemu on PATH, the AAVMF firmware, and the built fixture; the
        # in-module test skips without them so the plain `nextest` check passes
        # it over. Linux-only (Zot binary + loopback sandbox networking + TCG).
        qemu-boot = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-qemu-boot";
            version = "0.1.0";
            cargoArtifacts = cmn.testArtifacts;
            cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'test(boot_tiny_efi)'";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              cmn.zot
              pkgs.skopeo
              pkgs.qemu
            ];

            # oci-client initializes a TLS backend even for the plain-HTTP
            # loopback pulls; without a CA bundle that init panics (same as the
            # oci-store check). The connections are HTTP to 127.0.0.1.
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

            # AAVMF (aarch64 UEFI) firmware: code (read-only) + variable-store
            # template (copied writable by the test), shipped by the qemu pkg.
            TML_AAVMF_CODE = "${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd";
            TML_AAVMF_VARS = "${pkgs.qemu}/share/qemu/edk2-arm-vars.fd";

            TINY_EFI_IMAGE = self'.packages.tiny-efi-image-layout;
          }
        );

        # Log streaming (doc/log-streaming-plan.md phase 3): the live NATS
        # round-trips that can't run in the restricted sandbox (nats-server
        # binds a TCP port; AGENTS.md §2). Spins up a real `nats-server -js`
        # per test on loopback and runs the two `nats_live_*` tests:
        #   - supervisor-lib: spill → ship (publish-with-headers + ack) → read,
        #     asserting subject/headers/payloads round-trip;
        #   - switchboard: `NatsLogStreamProvisioner::ensure_job_stream` actually
        #     creates the per-job stream idempotently (backfills the phase-2
        #     deliverable left unwritten for the same sandbox reason).
        # Both tests skip when TML_TEST_NATS_SERVER is unset, so the plain
        # `nextest` check passes them over. Linux-only (loopback sandbox net).
        nats-log-streaming = cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-nats-log-streaming";
            version = "0.1.0";
            cargoArtifacts = cmn.testArtifacts;
            cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'test(nats_live)'";
            partitions = 1;
            partitionType = "count";

            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ [
              pkgs.nats-server
            ];

            # async-nats initializes a rustls backend even for the plain-text
            # loopback (nats://) connection; without a CA bundle that init can
            # panic (same posture as the oci-store check). Traffic is to
            # 127.0.0.1.
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

            # The nats-server binary the tests spawn; its presence also gates
            # them (unset elsewhere → skipped).
            TML_TEST_NATS_SERVER = "${pkgs.nats-server}/bin/nats-server";
          }
        );
      }
      # Promote each package output to a check so `nix flake check`
      # verifies they all build — EXCEPT the producer-side OCI image outputs
      # (`image-*` and the `images-parse` drift guard). Those are heavy
      # distro/TCG builds that must never gate ordinary PRs or the merge
      # queue (doc/images-oci-migration-plan.md §7); the dedicated
      # `.github/workflows/images.yml` builds them explicitly instead.
      // (lib.filterAttrs (
        name: _: !(lib.hasPrefix "image-" name || name == "images-parse")
      ) self'.packages);
    };
}
