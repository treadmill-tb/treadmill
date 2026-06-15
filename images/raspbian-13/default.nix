# raspbian-13 base image (arm64, nbd-netboot) — boot FAT + root qcow2.
#
# The "customize RPi OS under TCG qemu" build and blob extraction live in
# ./parts.nix (shared with ./gha-runner.nix); this file just wraps the boot FAT
# + root qcow2 blobs as a `mkTreadmillImage` OCI layout. Port of the legacy
# `images/netboot-raspberrypi-nbd/default.nix`.
#
# OCI-migration changes vs. the original:
#   * the puppet binary comes from the in-tree static aarch64 musl package
#     (passed into ./parts.nix) instead of the bespoke fenix build;
#   * `rustup-init` / the RPi OS image / nbd-client deb are `pkgs.fetchurl` FODs
#     (offline-eval-safe), not `builtins.fetchurl`;
#   * the boot blob is the raw `0.fat` partition stored DIRECTLY (the legacy tar
#     of the FAT contents is dropped — plan I10), `mediaType=bootFatV1`,
#     `role="boot"`;
#   * the bespoke TOML store shell is replaced by `mkTreadmillImage`, which wires
#     `head` = root digest and leaves the boot layer standalone.
{
  mediaTypes,
  mkTreadmillImage,
  # The shared build parts (import of ./parts.nix): { bootFat; rootQcow2; ... }.
  parts,
}:
# Boot FAT layer is standalone; the single root qcow2 is the head.
mkTreadmillImage {
  name = "raspbian-13";
  title = "Raspberry Pi OS 13 (NBD)";
  layers = [
    {
      path = "${parts.bootFat}";
      mediaType = mediaTypes.bootFatV1;
      role = "boot";
    }
    {
      path = "${parts.rootQcow2}";
      mediaType = mediaTypes.diskQcow2;
      role = "root";
      # null -> the helper reads the qcow2 virtual size with `qemu-img info`.
      virtualSize = null;
    }
  ];
}
