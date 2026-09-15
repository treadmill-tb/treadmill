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

      mkNextest =
        {
          tools ? [ ],
          ...
        }@args:
        cmn.craneLib.cargoNextest (
          cmn.cargoCommonArgs
          // {
            version = "0.1.0";
            cargoArtifacts = cmn.testArtifacts;
            partitions = 1;
            partitionType = "count";
            doInstallCargoArtifacts = false;
            nativeBuildInputs = cmn.cargoCommonArgs.nativeBuildInputs ++ tools;
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          }
          // removeAttrs args [ "tools" ]
        );

      fastChecks = {
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

        # Validate the committed switchboard and supervisor daemon OpenAPI specs
        # against the OpenAPI 3.1 schema. The drift tests (`openapi_spec`,
        # `daemon_api_spec`) keep these files in sync with the code; this check
        # additionally guarantees they are valid OpenAPI documents. `openapi-spec-validator` bundles its schemas, so it
        # runs offline in the build sandbox.
        openapi-spec = pkgs.runCommand "treadmill-openapi-spec-valid" { } ''
          ${pkgs.python3Packages.openapi-spec-validator}/bin/openapi-spec-validator \
            ${../switchboard/api-spec/openapi.yaml}
          ${pkgs.python3Packages.openapi-spec-validator}/bin/openapi-spec-validator \
            ${../supervisor/lib/api-spec/daemon-api.yaml}
          touch $out
        '';

        clippy = cmn.craneLib.cargoClippy (
          cmn.cargoCommonArgs
          // {
            pname = "treadmill-workspace";
            version = "0.1.0";
            cargoArtifacts = cmn.workspaceDeps;
            cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
            doInstallCargoArtifacts = false;
          }
        );

        # Run the workspace test suite via cargo-nextest. Scoped to
        # `--workspace` (NOT `--all-targets`): the binaries are built
        # separately by `mkBin`, so there's no need to rebuild them here.
        # `--no-tests=pass` keeps this green while the workspace has no
        # `#[test]` targets yet; remove it once tests exist and you'd rather a
        # crate accidentally losing all its tests be a CI failure.
        #
        # Tests that need external services (e.g. a Postgres for switchboard)
        # will need wiring here — see `switchboard-migrations-consistency`
        # below for the ephemeral-pg pattern that works inside the Nix
        # sandbox.
        nextest = mkNextest {
          pname = "treadmill-nextest";
          cargoNextestExtraArgs = "--workspace --no-tests=pass";
        };

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
        nextest-db = mkNextest {
          pname = "treadmill-nextest-db";
          cargoNextestExtraArgs = "--workspace --run-ignored only --no-tests=pass";
          tools = [ pkgs.postgresql ];

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
        };

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

        inherit (self'.packages) console;
      };

      heavyTests = {
        # TODO: Placeholder for end-to-end integration tests.
        integration-tests = pkgs.runCommand "integration-tests-todo" { } ''
          echo "TODO: end-to-end integration tests"
          mkdir -p $out
        '';
      }
      # Build the `tiny-efi` fixture and reparse its real wire-format
      # manifest through our `oci-spec`/`parse.rs` view. The `tiny_efi`
      # integration test skips when `TINY_EFI_IMAGE` is unset (so the plain
      # `nextest` check above passes it over); here we point it at the built
      # layout so it does its work. Linux-only: the fixture needs the Linux
      # packing/cross toolchain (see nix/tiny-efi.nix).
      // lib.optionalAttrs pkgs.stdenv.isLinux {
        tiny-efi-image = mkNextest {
          pname = "treadmill-tiny-efi-image";
          cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'binary(tiny_efi)'";
          TINY_EFI_IMAGE = self'.packages.tiny-efi-image-layout;
        };

        # Append a third qcow2 layer onto the `tiny-efi` fixture and drive
        # `image-util append` + `verify` over the result. Hermetic and seconds
        # long (no distro download), so image-format work is gated on every PR.
        image-util-add-a-layer =
          pkgs.runCommand "image-util-add-a-layer"
            {
              nativeBuildInputs = [
                self'.packages.image-util
                pkgs.qemu-utils
                pkgs.coreutils
                pkgs.findutils
                pkgs.gnugrep
              ];
            }
            ''
              set -euo pipefail

              lower="${self'.packages.tiny-efi-image-layout}"
              rev2="${self'.packages.tiny-efi-rev2-qcow2}/rev2.qcow2"

              image-util verify "$lower" --chain disk=2 \
                --title tiny-efi --name tiny-efi-lower

              image-util append --lower "$lower" --layer "disk=qcow2:$rev2" \
                --title "tiny-efi rev2" -o stacked
              image-util verify stacked --chain disk=3 \
                --title "tiny-efi rev2" --name tiny-efi-stacked

              # Every layer blob the append inherited keeps its digest, which is
              # what makes siblings of one lower dedupe in a registry.
              inherited=0
              for blob in "$lower"/blobs/sha256/*; do
                name="$(basename "$blob")"
                [ -e "stacked/blobs/sha256/$name" ] || continue
                cmp "$blob" "stacked/blobs/sha256/$name"
                inherited=$((inherited + 1))
              done
              [ "$inherited" -ge 3 ] || {
                echo "expected both layer blobs and the empty config to carry through" >&2
                exit 1
              }

              # Appending in place must leave no unreferenced manifest behind.
              cp -r --no-preserve=mode "$lower" inplace
              image-util append --lower inplace --layer "disk=qcow2:$rev2" \
                --title "tiny-efi rev2" -o inplace
              image-util verify inplace --chain disk=3 \
                --title "tiny-efi rev2" --name tiny-efi-inplace
              blobs="$(find inplace/blobs/sha256 -type f | wc -l)"
              [ "$blobs" = 5 ] || {
                echo "in-place append left $blobs blobs, expected 5" >&2
                find inplace/blobs/sha256 -type f >&2
                exit 1
              }

              # A verify that cannot fail is worthless: corrupt one blob's
              # bytes without changing its length and confirm it is caught.
              refute() { # <message> <layout>
                if image-util verify "$2" --name refute 2>err.log; then
                  echo "verify accepted $1" >&2
                  exit 1
                fi
                grep -q "$3" err.log || {
                  echo "verify rejected $1 for the wrong reason:" >&2
                  cat err.log >&2
                  exit 1
                }
              }

              cp -r --no-preserve=mode stacked corrupt
              head_blob="$(ls -S corrupt/blobs/sha256/* | head -n1)"
              printf 'x' | dd of="$head_blob" bs=1 seek=100000 conv=notrunc status=none
              refute "a blob whose bytes no longer hash to its digest" corrupt \
                "hashes to"

              # A baked backing file is what blockdev.rs's base node forbids.
              cp -r --no-preserve=mode stacked baked
              baked_head="$(ls -S baked/blobs/sha256/* | head -n1)"
              qemu-img rebase -u -b /nonexistent.qcow2 -F qcow2 -f qcow2 "$baked_head"
              # The rebase changed the blob, so re-assemble around the new bytes.
              image-util assemble --title baked --layer "disk=qcow2:$baked_head" -o baked-layout
              refute "a blob with a baked backing_file" baked-layout \
                "baked backing_file"

              touch $out
            '';

        # Build an image with two independent chains, the way an nbd-netboot
        # image carries `bootfs` and `rootfs`, and grow both. Pins that roles
        # carry no special meaning to the tooling, that a qcow2 layer can back
        # onto a raw base, and that the manifest does not depend on the order
        # layers are given in.
        image-util-roles =
          pkgs.runCommand "image-util-roles"
            {
              nativeBuildInputs = [
                self'.packages.image-util
                pkgs.qemu-utils
                pkgs.coreutils
                pkgs.gnugrep
                pkgs.jq
              ];
            }
            ''
              set -euo pipefail

              # fill <file> <bytes> <char>: a file of <bytes> copies of <char>.
              fill() { head -c "$2" /dev/zero | tr '\0' "$3" >"$1"; }
              qcow2() { # <out> <bytes> <char>
                fill "$1.raw" "$2" "$3"
                qemu-img convert -f raw -O qcow2 "$1.raw" "$1"
              }

              fill boot0.raw 1048576 b
              qcow2 boot1.qcow2 1048576 B
              qcow2 root0.qcow2 4194304 r
              qcow2 root1.qcow2 8388608 R

              image-util assemble --title roles \
                --layer bootfs=raw:boot0.raw --layer rootfs=qcow2:root0.qcow2 -o base
              image-util verify base --chain bootfs=1 --chain rootfs=1 --name base

              image-util assemble --title roles \
                --layer rootfs=qcow2:root0.qcow2 --layer bootfs=raw:boot0.raw -o reordered
              cmp base/index.json reordered/index.json

              image-util append --lower base \
                --layer rootfs=qcow2:root1.qcow2 --layer bootfs=qcow2:boot1.qcow2 -o derived
              image-util verify derived --chain bootfs=2 --chain rootfs=2 --name derived

              blob() { echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; }
              manifest="derived/blobs/sha256/$(jq -r '.manifests[0].digest | sub("^sha256:"; "")' derived/index.json)"
              jq -e \
                --arg boot0 "$(blob boot0.raw)" --arg boot1 "$(blob boot1.qcow2)" \
                --arg root0 "$(blob root0.qcow2)" --arg root1 "$(blob root1.qcow2)" '
                [.layers[] | [.digest, .mediaType, .annotations["dev.treadmill.role"], .annotations["dev.treadmill.qcow2.lower"]]]
                == [
                  [$boot0, "application/vnd.treadmill.raw", null, null],
                  [$boot1, "application/vnd.treadmill.qcow2", "bootfs", $boot0],
                  [$root0, "application/vnd.treadmill.qcow2", null, null],
                  [$root1, "application/vnd.treadmill.qcow2", "rootfs", $root0]
                ]
              ' "$manifest" >/dev/null

              fails() { # <message> <expected error> <command...>
                local message="$1" expected="$2"
                shift 2
                if "$@" 2>err.log; then
                  echo "image-util accepted $message" >&2
                  exit 1
                fi
                grep -q "$expected" err.log || {
                  echo "image-util rejected $message for the wrong reason:" >&2
                  cat err.log >&2
                  exit 1
                }
              }

              fill other.raw 1048576 o
              fails "a raw blob on top of a chain" "cannot go on top" \
                image-util append --lower base --layer bootfs=raw:other.raw -o raw-on-top
              fails "a raw blob declared as qcow2" "qemu-img info failed" \
                image-util assemble --title roles --layer bootfs=qcow2:boot0.raw -o mislabeled
              fails "a chain of the wrong length" "unexpected chains" \
                image-util verify derived --chain bootfs=2 --chain rootfs=1
              fails "a missing role" "unexpected chains" \
                image-util verify derived --chain rootfs=2

              touch $out
            '';

        # Drive the `oci_store` client against a real child Zot. The tests spin up Zot
        # (and a second one as a copy source) on loopback and skopeo the
        # `tiny-efi` fixture in, so the check needs zot + skopeo on PATH and the
        # built fixture in TINY_EFI_IMAGE. Like the reparse test, the oci_store
        # tests skip when those are unset, so the plain `nextest` check passes
        # them over. Linux-only (Zot binary + loopback sandbox networking).
        oci-store = mkNextest {
          pname = "treadmill-oci-store";
          # The leases-as-references tests also live in `oci_store::tests` but
          # drive GC and take tens of seconds each; they have their own `lease`
          # check below, so exclude them here.
          cargoNextestExtraArgs =
            "--workspace --no-tests=pass "
            + "-E 'test(oci_store) & !test(lease_pins_against_gc) & !test(parallel_ensure_present_while_pinned)'";
          tools = [
            cmn.zot
            pkgs.skopeo
          ];
          TINY_EFI_IMAGE = self'.packages.tiny-efi-image-layout;
        };

        # Prove the leases-as-references model against the real Zot binary. The tests pin
        # an `inuse-<job>` reference, drive Zot's GC, and assert the pinned
        # closure is retained while an unreferenced image is collected (and that
        # releasing the lease makes the closure collectible). Same external needs
        # as `oci-store` (zot + skopeo + the fixture); the tests skip when unset
        # so the plain `nextest` check passes them over. Linux-only.
        lease = mkNextest {
          pname = "treadmill-lease";
          cargoNextestExtraArgs =
            "--workspace --no-tests=pass "
            + "-E 'test(lease_pins_against_gc) | test(parallel_ensure_present_while_pinned)'";
          tools = [
            cmn.zot
            pkgs.skopeo
          ];
          TINY_EFI_IMAGE = self'.packages.tiny-efi-image-layout;
        };

        # Validate the backing-chain emitter against real qemu — assemble the `-blockdev`
        # node graph with qemu-storage-daemon, export it over NBD, and read it
        # back with qemu-io. Needs the qemu tools on PATH; the test skips
        # without them so the plain `nextest` check passes it over.
        chain-assembly = mkNextest {
          pname = "treadmill-chain-assembly";
          cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'binary(chain_assembly)'";
          tools = [ pkgs.qemu ];
        };

        netboot-daemons = mkNextest {
          pname = "treadmill-netboot-daemons";
          cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'test(real_daemons)'";
          tools = [
            pkgs.qemu-utils
            pkgs.dosfstools
            pkgs.mtools
            cmn.nbdfatftpd
          ];
        };

        # Log streaming: the live NATS round-trips that can't run in the
        # restricted sandbox (nats-server binds a TCP port; AGENTS.md §2).
        # Spins up a real `nats-server -js`
        # per test on loopback and runs the two `nats_live_*` tests:
        #   - supervisor-lib: spill → ship (publish-with-headers + ack) → read,
        #     asserting subject/headers/payloads round-trip;
        #   - switchboard: `NatsLogStreamProvisioner::ensure_job_stream` actually
        #     creates the per-job stream idempotently (backfills the phase-2
        #     deliverable left unwritten for the same sandbox reason).
        # Both tests skip when TML_TEST_NATS_SERVER is unset, so the plain
        # `nextest` check passes them over. Linux-only (loopback sandbox net).
        nats-log-streaming = mkNextest {
          pname = "treadmill-nats-log-streaming";
          cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'test(nats_live)'";
          tools = [ pkgs.nats-server ];

          # The nats-server binary the tests spawn; its presence also gates
          # them (unset elsewhere → skipped).
          TML_TEST_NATS_SERVER = "${pkgs.nats-server}/bin/nats-server";
        };
      };

      heavyPackages = lib.attrVals (
        [
          "tml"
          "swx"
          "image-util"
          "treadmill-qemu-supervisor"
          "treadmill-nbd-netboot-supervisor"
          "zot"
          "nbdfatftpd"
          "job-gateway-caddy"
        ]
        ++ lib.optionals pkgs.stdenv.isLinux [
          "tml-static-x86_64"
          "tml-caddy-static-x86_64"
          "tml-caddy-static-aarch64"
          "tiny-efi-app-base"
          "tiny-efi-app-rev1"
          "tiny-efi-app-rev2"
          "tiny-efi-rev2-qcow2"
          "tiny-efi-image-layout"
        ]
      ) self'.packages;
    in
    {
      checks = fastChecks;

      packages = heavyTests // {
        ci-heavy = pkgs.linkFarmFromDrvs "treadmill-ci-heavy" (lib.attrValues heavyTests ++ heavyPackages);

        cache-seed = pkgs.linkFarmFromDrvs "treadmill-cache-seed" (
          [
            # The dependency layer discards its references, so the toolchain
            # and the vendored crate sources need listing on their own.
            cmn.rustToolchain
            cmn.cargoVendorDir
            cmn.workspaceDeps
            cmn.zot
            cmn.nbdfatftpd
            cmn.job-gateway-caddy
          ]
          ++ lib.attrVals (lib.optionals pkgs.stdenv.isLinux [
            "tml-caddy-static-x86_64"
            "tml-caddy-static-aarch64"
          ]) self'.packages
        );
      };
    };
}
