#!/bin/sh
# Raspberry Pi OS pre-install hook: runs in-guest BEFORE the manifest `packages`
# are apt-installed (build-image.sh, both backends). The guest kernel is NEVER
# booted.
#
# Installing nbd-client triggers `update-initramfs`, which under the default
# Raspberry Pi OS `MODULES=dep` tries to probe the *current* root device to pick
# modules — and fails in our no-boot build environment (the nspawn container's /
# is the NBD-mapped delta; libguestfs's appliance has the same problem):
#
#   mkinitramfs: failed to determine device for /
#
# MODULES=most builds a portable initramfs that bundles a broad module set
# (including nbd + the network drivers) without probing the root device — which
# is exactly what an NBD-netboot image needs anyway. Set as a conf.d drop-in so
# it overrides initramfs.conf without editing it.
set -eu

mkdir -p /etc/initramfs-tools/conf.d
cat >/etc/initramfs-tools/conf.d/10-tml-netboot <<'CONF'
# Treadmill NBD-netboot image: build a portable initramfs (see pre-install.sh).
MODULES=most
CONF
