# Producer-side OCI image builds (doc/images-oci-migration-plan.md).
#
# This module folds the legacy standalone `images/` repository into the
# monorepo: it builds the Treadmill production images as proper OCI layouts via
# the shared `images/lib/` helpers (the siblings of the `nix/tiny-efi.nix`
# reference fixture) and exposes them as `packages.image-*`.
#
# Linux-only (exactly like nix/tiny-efi.nix): the builds need `runInLinuxVM`,
# `qemu-system-*`, `dosfstools`/`mtools`, deb bootstrapping, etc.
#
# Heavy builds are kept OUT of the ordinary `nix flake check` set: the image /
# images-parse outputs are excluded from the package→check promotion in
# nix/checks.nix, so they gate nothing on PRs or the merge queue. They are built
# explicitly by the separate `.github/workflows/images.yml` workflow via
# `nix build .#packages.<sys>.{image-*,images-parse}`.
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
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;

      cmn = import ./lib.nix { inherit inputs system pkgs; };

      mediaTypes = import ../images/lib/media-types.nix;
      mkTreadmillImage = import ../images/lib/mk-treadmill-image.nix { inherit pkgs lib mediaTypes; };

      # --- Phase A smoke ----------------------------------------------------
      # Prove `mkTreadmillImage` against REAL qcow2 blobs before any heavy distro
      # build exists. Self-contained: build a throwaway two-layer qcow2 backing
      # chain from scratch (no cross-module package reference), package it
      # through the helper, and reparse the result via the images-parse guard.
      smokeBlobs =
        pkgs.runCommand "treadmill-image-helper-smoke-blobs" { nativeBuildInputs = [ pkgs.qemu-utils ]; }
          ''
            set -euo pipefail
            mkdir -p "$out"
            qemu-img create -f qcow2 "$out/base.qcow2" 16M
            qemu-io -c "write -P 0x11 0 64k" "$out/base.qcow2"
            # A real differential overlay over the base, with the baked backing
            # path then stripped (`rebase -u -b ""`) — mirrors the runner-overlay
            # recipe, so the helper's chain wiring is exercised end to end. Write
            # a distinct pattern so the overlay genuinely differs from the base.
            qemu-img create -f qcow2 -b "$out/base.qcow2" -F qcow2 full.qcow2
            qemu-io -c "write -P 0x22 0 64k" full.qcow2
            qemu-img convert -O qcow2 -B "$out/base.qcow2" -F qcow2 full.qcow2 overlay.qcow2
            qemu-img rebase -u -b "" -f qcow2 overlay.qcow2
            cp overlay.qcow2 "$out/overlay.qcow2"
          '';

      imageHelperSmoke = mkTreadmillImage {
        name = "helper-smoke";
        title = "helper-smoke";
        layers = [
          {
            path = "${smokeBlobs}/base.qcow2";
            mediaType = mediaTypes.diskQcow2;
            role = "root";
            # Left null on purpose so the helper's `qemu-img info` virtual-size
            # path is exercised too.
            virtualSize = null;
          }
          {
            path = "${smokeBlobs}/overlay.qcow2";
            mediaType = mediaTypes.diskQcow2;
            role = "root";
            virtualSize = null;
          }
        ];
      };

      # --- Image products ---------------------------------------------------
      # The image recipes consume the in-tree static `tml-puppet` packages
      # (nix/puppet-cross-musl.nix) via `self'.packages` — the normal
      # flake-parts cross-package reference (it does not cause the
      # `_module.args` recursion; that only happens when `optionalAttrs` gates
      # the whole perSystem config, which is why `packages` stays unconditional
      # below).
      # The deb-bootstrapped rootfs is shared between the base image and the
      # gha-runner overlay so the overlay's `lower` digest is byte-identical to
      # the base head blob (one derivation -> one store path -> one blob).
      ubuntuRootfs = import ../images/ubuntu-2204/rootfs.nix {
        inherit pkgs lib;
        puppet = self'.packages.tml-puppet-static-x86_64;
      };
      ubuntu-2204 = import ../images/ubuntu-2204/default.nix {
        inherit mediaTypes mkTreadmillImage;
        rootfs = ubuntuRootfs;
      };
      ubuntu-2204-gha-runner = import ../images/ubuntu-2204-gha-runner/default.nix {
        inherit
          pkgs
          lib
          mediaTypes
          mkTreadmillImage
          ;
        baseRootfs = ubuntuRootfs;
      };

      # The customized SD image + boot/root blobs are shared between the base
      # image and the gha-runner overlay (one derivation set -> one blob each),
      # so the overlay's `lower` digest is byte-identical to the base head blob.
      raspbianParts = import ../images/raspbian-13/parts.nix {
        inherit pkgs lib;
        puppet = self'.packages.tml-puppet-static-aarch64;
      };
      raspbian-13 = import ../images/raspbian-13/default.nix {
        inherit mediaTypes mkTreadmillImage;
        parts = raspbianParts;
      };
      raspbian-13-gha-runner = import ../images/raspbian-13/gha-runner.nix {
        inherit
          pkgs
          lib
          mediaTypes
          mkTreadmillImage
          ;
        parts = raspbianParts;
      };

      # name -> { layout; rootLayers; bootLayers; title; }
      imageDefs = {
        ubuntu-2204 = {
          layout = ubuntu-2204;
          rootLayers = 1;
          bootLayers = 0;
          title = "Ubuntu 22.04";
        };
        ubuntu-2204-gha-runner = {
          layout = ubuntu-2204-gha-runner;
          rootLayers = 2;
          bootLayers = 0;
          title = "Ubuntu 22.04 with GitHub Actions Runner";
        };
        raspbian-13 = {
          layout = raspbian-13;
          rootLayers = 1;
          bootLayers = 1;
          title = "Raspberry Pi OS 13 (NBD)";
        };
        raspbian-13-gha-runner = {
          layout = raspbian-13-gha-runner;
          rootLayers = 2;
          bootLayers = 1;
          title = "Raspberry Pi OS 13 (NBD) with GitHub Actions Runner";
        };
      };
      # Smoke entries that exercise the helpers but are not shipped products.
      smokeImageDefs = {
        helper-smoke = {
          layout = imageHelperSmoke;
          rootLayers = 2;
          bootLayers = 0;
          title = "helper-smoke";
        };
      };
      allImageDefs = smokeImageDefs // imageDefs;

      # --- images-parse drift guard (§6.1) ----------------------------------
      # JSON spec consumed by treadmill-rs/tests/images_parse.rs: every built
      # layout + its expected shape. snake_case keys to match the serde structs.
      parseSpec = pkgs.writeText "images-parse-spec.json" (
        builtins.toJSON {
          images = lib.mapAttrsToList (name: d: {
            inherit name;
            path = d.layout;
            root_layers = d.rootLayers;
            boot_layers = d.bootLayers;
            inherit (d) title;
          }) allImageDefs;
        }
      );

      # Reuse the shared test-binary layer; only RUN the images_parse binary,
      # pointed at the spec above (same shape as the tiny-efi-image check). This
      # is a package, not a check, so the ordinary `nix flake check` never pulls
      # the heavy image layouts it references (see module header).
      imagesParse = cmn.craneLib.cargoNextest (
        cmn.cargoCommonArgs
        // {
          pname = "treadmill-images-parse";
          version = "0.1.0";
          cargoArtifacts = cmn.testArtifacts;
          cargoNextestExtraArgs = "--workspace --no-tests=pass -E 'binary(images_parse)'";
          partitions = 1;
          partitionType = "count";

          TML_IMAGES_PARSE_SPEC = parseSpec;
        }
      );

      imagePackages = lib.mapAttrs' (name: d: lib.nameValuePair "image-${name}" d.layout) imageDefs;
    in
    {
      # `optionalAttrs` lives INSIDE `packages` (not around the whole config):
      # gating the entire perSystem return on `pkgs.stdenv.isLinux` makes
      # flake-parts force this module's config to resolve `_module.args.pkgs`,
      # which needs `pkgs` — an infinite recursion. Keeping the `packages` key
      # unconditional (its value lazy) breaks the cycle, exactly like
      # nix/tiny-efi.nix. On non-Linux systems `packages` is simply empty.
      packages = lib.optionalAttrs isLinux (
        imagePackages
        // {
          image-helper-smoke = imageHelperSmoke;
          images-parse = imagesParse;
        }
      );
    };
}
