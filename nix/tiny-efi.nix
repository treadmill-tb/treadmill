# The `tiny-efi` test fixture (doc/oci-image-migration-plan.md §12.2).
#
# A minimal UEFI app is built twice from one source (a base printing the
# `BASE-ONLY` tripwire and an overlay printing `TREADMILL_OK rev=1`), packed
# into FAT EFI System Partitions, converted to qcow2, and stacked into a
# genuine two-layer backing chain. The chain is then assembled into an OCI
# image layout carrying the Treadmill media types and annotations the rest of
# the system consumes, so later phases can push it to a registry (Phase 1) and
# boot it (Phase 2), and so Phase 0 can prove our `oci-spec`/`parse.rs` view
# round-trips against a real-wire-format manifest.
{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      ...
    }:
    let
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;
      inherit (inputs) fenix;

      fenixPkgs = fenix.packages.${system};

      efiTarget = "aarch64-unknown-uefi";

      # Stable toolchain plus the foreign UEFI target's prebuilt std.
      efiRust = fenixPkgs.combine [
        fenixPkgs.stable.rustc
        fenixPkgs.stable.cargo
        fenixPkgs.targets.${efiTarget}.stable.rust-std
      ];

      fixtureSrc = lib.fileset.toSource {
        root = ./fixtures/tiny-efi;
        fileset = lib.fileset.unions [
          ./fixtures/tiny-efi/Cargo.toml
          ./fixtures/tiny-efi/Cargo.lock
          ./fixtures/tiny-efi/src
        ];
      };

      # Vendored crate sources for the offline build (shared across the layers).
      cargoVendorDir = pkgs.rustPlatform.importCargoLock {
        lockFile = ./fixtures/tiny-efi/Cargo.lock;
      };

      # Build BOOTAA64.EFI with a compiled-in sentinel line. `name` only
      # disambiguates the derivations / store paths. We drive cargo directly
      # rather than via `buildRustPackage`, whose hooks force the host target;
      # this is a bare-metal cross build to `aarch64-unknown-uefi`.
      mkEfiApp =
        {
          name,
          sentinel,
        }:
        pkgs.stdenvNoCC.mkDerivation {
          pname = "tiny-efi-${name}";
          version = "0.0.0";
          src = fixtureSrc;

          cargoDeps = cargoVendorDir;
          nativeBuildInputs = [
            pkgs.rustPlatform.cargoSetupHook
            efiRust
          ];

          TINY_EFI_SENTINEL = sentinel;

          buildPhase = ''
            runHook preBuild
            cargo build --release --frozen --offline \
              --target ${efiTarget} --bin tiny-efi
            runHook postBuild
          '';

          # Install the artifact as the aarch64 removable-media default boot
          # path so firmware finds it without an explicit boot entry.
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp "target/${efiTarget}/release/tiny-efi.efi" "$out/BOOTAA64.EFI"
            runHook postInstall
          '';
        };

      efiBase = mkEfiApp {
        name = "base";
        sentinel = "BASE-ONLY";
      };
      efiRev1 = mkEfiApp {
        name = "rev1";
        sentinel = "TREADMILL_OK rev=1";
      };
      # Kept available for the Phase 5 add-a-layer test (rev=2 overlay).
      efiRev2 = mkEfiApp {
        name = "rev2";
        sentinel = "TREADMILL_OK rev=2";
      };

      packingTools = [
        pkgs.dosfstools
        pkgs.mtools
        pkgs.qemu-utils
      ];

      # Size of every ESP (and hence the virtual size of every qcow2 layer in a
      # chain — overlays must match their base). 16 MiB comfortably holds the
      # tiny EFI binary with room for FAT overhead.
      espBytes = 16 * 1024 * 1024;

      # Pack a single BOOTAA64.EFI into a FAT16 EFI System Partition image. No
      # mounting: mkfs.fat + mtools operate on the image file directly, so this
      # needs no privileges and runs in the Nix sandbox.
      mkEsp =
        {
          name,
          efiApp,
        }:
        pkgs.runCommand "tiny-efi-esp-${name}" { nativeBuildInputs = packingTools; } ''
          # Deterministic mtools: no timestamps from the host clock.
          export SOURCE_DATE_EPOCH=0
          truncate -s ${toString espBytes} esp.img
          mkfs.fat -F 16 -n EFIBOOT esp.img
          mmd -i esp.img ::/EFI
          mmd -i esp.img ::/EFI/BOOT
          mcopy -i esp.img "${efiApp}/BOOTAA64.EFI" ::/EFI/BOOT/BOOTAA64.EFI
          mkdir -p "$out"
          cp esp.img "$out/esp.img"
        '';

      espBase = mkEsp {
        name = "base";
        efiApp = efiBase;
      };
      espRev1 = mkEsp {
        name = "rev1";
        efiApp = efiRev1;
      };

      # The base qcow2 blob: a full-content image converted straight from the
      # base ESP.
      baseQcow2 = pkgs.runCommand "tiny-efi-base-qcow2" { nativeBuildInputs = [ pkgs.qemu-utils ]; } ''
        mkdir -p "$out"
        qemu-img convert -f raw -O qcow2 "${espBase}/esp.img" "$out/base.qcow2"
      '';

      # The overlay qcow2 blob: only the clusters that differ from the base
      # (the rewritten BOOTAA64.EFI plus the FAT/dir metadata that moves with
      # it). `convert -B` writes just the differences against the base; the
      # `rebase -u -b ""` then strips the baked backing reference so the shared
      # blob names no path — the chain is supplied at runtime (D3). Read alone
      # the overlay is an incomplete FAT (a tripwire for a missing base); only
      # stacked over the base does it yield the rev=1 ESP.
      overlayQcow2 =
        pkgs.runCommand "tiny-efi-overlay-qcow2" { nativeBuildInputs = [ pkgs.qemu-utils ]; }
          ''
            mkdir -p "$out"
            qemu-img convert -O qcow2 -B "${baseQcow2}/base.qcow2" -F qcow2 \
              "${espRev1}/esp.img" overlay.qcow2
            qemu-img rebase -u -b "" -f qcow2 overlay.qcow2
            cp overlay.qcow2 "$out/overlay.qcow2"
          '';

      # Assemble the OCI image layout by hand: we need a pure-artifact manifest
      # (empty config + Treadmill artifactType), custom blob media types, and
      # per-layer/manifest `ci.treadmill.*` annotations that umoci/oras don't
      # express conveniently. The result is a standard OCI image layout
      # (oci-layout + index.json + blobs/sha256/<digest>).
      tinyEfiImageLayout =
        pkgs.runCommand "tiny-efi-image-layout"
          {
            nativeBuildInputs = [
              pkgs.jq
              pkgs.qemu-utils
              pkgs.coreutils
            ];
            passthru = {
              title = "tiny-efi";
            };
          }
          ''
            set -euo pipefail

            base_qcow2="${baseQcow2}/base.qcow2"
            overlay_qcow2="${overlayQcow2}/overlay.qcow2"

            blobs="$out/blobs/sha256"
            mkdir -p "$blobs"

            store() {
              # store <file> -> echoes the sha256 hex and copies it into the CAS
              local f="$1" d
              d="$(sha256sum "$f" | cut -d' ' -f1)"
              cp "$f" "$blobs/$d"
              printf '%s' "$d"
            }
            size() { stat -c%s "$1"; }
            vsize() { qemu-img info --output=json "$1" | jq '."virtual-size"'; }

            # Empty config marks the manifest as a pure artifact.
            printf '{}' > config.json
            cfg_digest="$(store config.json)"
            cfg_size="$(size config.json)"

            base_digest="$(store "$base_qcow2")"
            base_size="$(size "$base_qcow2")"
            base_vsize="$(vsize "$base_qcow2")"

            overlay_digest="$(store "$overlay_qcow2")"
            overlay_size="$(size "$overlay_qcow2")"
            overlay_vsize="$(vsize "$overlay_qcow2")"

            jq -n \
              --arg cfg "sha256:$cfg_digest" --argjson cfgsize "$cfg_size" \
              --arg base "sha256:$base_digest" --argjson basesize "$base_size" \
              --arg basevs "$base_vsize" \
              --arg overlay "sha256:$overlay_digest" --argjson overlaysize "$overlay_size" \
              --arg overlayvs "$overlay_vsize" \
              '{
                schemaVersion: 2,
                mediaType: "application/vnd.oci.image.manifest.v1+json",
                artifactType: "application/vnd.treadmill.image.v1+json",
                config: {
                  mediaType: "application/vnd.oci.empty.v1+json",
                  digest: $cfg,
                  size: $cfgsize,
                  data: "e30="
                },
                layers: [
                  {
                    mediaType: "application/vnd.treadmill.disk.qcow2",
                    digest: $base,
                    size: $basesize,
                    annotations: {
                      "ci.treadmill.role": "root",
                      "ci.treadmill.qcow2.virtual-size": $basevs
                    }
                  },
                  {
                    mediaType: "application/vnd.treadmill.disk.qcow2",
                    digest: $overlay,
                    size: $overlaysize,
                    annotations: {
                      "ci.treadmill.role": "root",
                      "ci.treadmill.qcow2.virtual-size": $overlayvs,
                      "ci.treadmill.qcow2.lower": $base
                    }
                  }
                ],
                annotations: {
                  "org.opencontainers.image.title": "tiny-efi",
                  "ci.treadmill.qcow2.head": $overlay
                }
              }' > manifest.json

            manifest_digest="$(store manifest.json)"
            manifest_size="$(size manifest.json)"

            printf '{"imageLayoutVersion":"1.0.0"}' > "$out/oci-layout"

            jq -n \
              --arg manifest "sha256:$manifest_digest" --argjson manifestsize "$manifest_size" \
              '{
                schemaVersion: 2,
                mediaType: "application/vnd.oci.image.index.v1+json",
                manifests: [
                  {
                    mediaType: "application/vnd.oci.image.manifest.v1+json",
                    artifactType: "application/vnd.treadmill.image.v1+json",
                    digest: $manifest,
                    size: $manifestsize
                  }
                ]
              }' > "$out/index.json"
          '';
    in
    {
      # The fixture relies on a Linux toolchain (FAT packing via dosfstools/
      # mtools, qcow2 via qemu-utils, and a cross build of a UEFI binary). It is
      # only consumed by the Linux-only tests/checks of the OCI migration, so
      # keep it off the flake's Darwin systems entirely.
      packages = lib.optionalAttrs isLinux {
        tiny-efi-app-base = efiBase;
        tiny-efi-app-rev1 = efiRev1;
        tiny-efi-app-rev2 = efiRev2;
        tiny-efi-image-layout = tinyEfiImageLayout;
      };
    };
}
