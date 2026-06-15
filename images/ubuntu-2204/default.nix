# ubuntu-2204 base image (amd64, qemu/uefi) — 1 root layer.
#
# The deb-bootstrapped root filesystem lives in ./rootfs.nix (shared with the
# gha-runner overlay); this file just wraps its `disk-image.qcow2` as a
# single-root-layer `mkTreadmillImage` OCI layout. Port of the legacy
# `images/vm-ubuntu-2204-amd64-uefi/default.nix` with the two OCI-migration
# changes documented on ./rootfs.nix (static `tml-puppet`, `pkgs.fetchurl`
# rustup-init) plus the bespoke TOML store shell replaced by the OCI helper.
{
  mediaTypes,
  mkTreadmillImage,
  # The shared deb-bootstrapped rootfs derivation (import of ./rootfs.nix);
  # `${rootfs}/disk-image.qcow2` is the compressed qcow2.
  rootfs,
}:
mkTreadmillImage {
  name = "ubuntu-2204";
  title = "Ubuntu 22.04";
  layers = [
    {
      path = "${rootfs}/disk-image.qcow2";
      mediaType = mediaTypes.diskQcow2;
      role = "root";
      # null -> the helper reads the qcow2 virtual size with `qemu-img info`.
      virtualSize = null;
    }
  ];
}
