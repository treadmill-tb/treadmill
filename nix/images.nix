# Producer-side OCI image builds.
#
# This module builds the legacy `vmTools`-based Treadmill production images as
# OCI layouts and exposes them as `packages.image-*`. The layouts are assembled
# and validated by the `image-util` binary (images/util); this is the parallel
# vmTools pipeline that the libguestfs pipeline replaces
# (doc/images-libguestfs-build-plan.md — the recipe files here go away in its
# Phase 6).
#
# Linux-only (exactly like nix/tiny-efi.nix): the builds need `runInLinuxVM`,
# `qemu-system-*`, `dosfstools`/`mtools`, deb bootstrapping, etc.
#
# Heavy builds are kept OUT of the ordinary `nix flake check` set: every output
# whose name starts with `image-` (the layouts, the per-image `image-check-*`
# drift guards, AND the `image-util` binary) is excluded from the package→check
# promotion in nix/checks.nix, so they gate nothing on PRs or the merge queue.
# They are built explicitly by the separate `.github/workflows/images.yml`
# workflow via `nix build .#packages.<sys>.{image-*,image-check-*}`. The
# `image-util` binary itself is cheap and image-independent; it is compiled by
# the workspace clippy check regardless.
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

      # `image-util` owns the OCI media-type / annotation constants and the OCI
      # layout assembly (`treadmill_rs::image::assemble`, symmetric with the
      # parser); this binary is its CLI. Compiling it is cheap and
      # image-independent.
      imageUtilBin = cmn.mkBin { bin = "image-util"; };

      # The per-image recipes below still tag each layer with a `mediaType`, but
      # it is vestigial: `image-util` derives the real media type from the layer
      # role. This 2-entry shim only keeps those references resolving until the
      # recipes are replaced by `images/lib/build-image.sh`
      # (doc/images-libguestfs-build-plan.md Phase 6).
      mediaTypes = {
        diskQcow2 = "application/vnd.treadmill.disk.qcow2";
        bootFatV1 = "application/vnd.treadmill.boot.fat.v1";
      };

      # Producer wrapper: assemble an ordered layer list into a validated OCI
      # layout via `image-util assemble`. Backing-chain wiring and media types
      # are derived in Rust (shared with the parser), so this just forwards
      # role + path in order; `mediaType`/`virtualSize` on layers are ignored
      # (the qcow2 virtual size is read from each blob's header).
      mkTreadmillImage =
        {
          name,
          title,
          version ? null,
          description ? null,
          layers,
        }:
        pkgs.runCommand "treadmill-image-${name}" { nativeBuildInputs = [ imageUtilBin ]; } ''
          image-util assemble \
            --title ${lib.escapeShellArg title} \
            ${lib.optionalString (version != null) "--version ${lib.escapeShellArg version}"} \
            ${lib.optionalString (description != null) "--description ${lib.escapeShellArg description}"} \
            ${
              lib.concatMapStringsSep " " (l: "--layer ${l.role}=${lib.escapeShellArg (toString l.path)}") layers
            } \
            -o "$out"
        '';

      # --- Phase A smoke ----------------------------------------------------
      # Prove `mkTreadmillImage` against REAL qcow2 blobs before any heavy distro
      # build exists. Self-contained: build a throwaway two-layer qcow2 backing
      # chain from scratch (no cross-module package reference), package it
      # through the helper, and reparse the result via `image-util parse`.
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

      # --- per-image drift guard --------------------------------------------
      # `image-util parse` reparses ONE built layout through the real
      # `treadmill_rs::image::parse` view and asserts its expected shape. The
      # heavy work is the layout each check points at, not the shared binary.

      # One check derivation per layout, depending on ONLY that layout (so the
      # whole image set never has to build at once — that was the wart in the old
      # single `images-parse` spec). Built explicitly by the images workflow, and
      # excluded from `nix flake check` via the `image-` prefix (see
      # nix/checks.nix) since each pulls a heavy image build.
      imageChecks = lib.mapAttrs' (
        name: d:
        lib.nameValuePair "image-check-${name}" (
          pkgs.runCommand "image-check-${name}" { } ''
            ${imageUtilBin}/bin/image-util parse ${d.layout} \
              --name ${lib.escapeShellArg name} \
              --root-layers ${toString d.rootLayers} \
              --boot-layers ${toString d.bootLayers} \
              --title ${lib.escapeShellArg d.title}
            touch "$out"
          ''
        )
      ) allImageDefs;

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
        // imageChecks
        // {
          image-helper-smoke = imageHelperSmoke;
          image-util = imageUtilBin;
        }
      );
    };
}
