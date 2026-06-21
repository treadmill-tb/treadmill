# images/ubuntu-server-2604/manifest.sh — data only, sourced by build-image.sh.
#
# Ubuntu Server 26.04 LTS (resolute), amd64, qemu/uefi. Built from the upstream
# cloud image (libguestfs pipeline; see doc/images-libguestfs-build-plan.md).
arch="x86_64"
type="disk" # whole-disk qcow2 base
title="Ubuntu Server 26.04"
version="26.04"

# Upstream cloud image: the amd64 cloudimg is already a qcow2, so it is used
# verbatim as the lowest, content-addressed root layer (the dedupe blob). Pinned
# to a dated release for a stable URL + checksum (`release/` is a moving
# symlink). The sha256 is verified on every fetch as a drift guard.
base_image_url="https://cloud-images.ubuntu.com/releases/26.04/release-20260612/ubuntu-26.04-server-cloudimg-amd64.img"
base_image_sha256="0c9fb915bab0b36b361d3bf8aeae2115dda19d81a306656964de048033481670"

# rustup-init (x86_64): installed for the tml user, no default toolchain. Pinned
# to a versioned archive on static.rust-lang.org (stable URL + hash); the sha256
# is verified on every fetch.
rustup_init_url="https://static.rust-lang.org/rustup/archive/1.29.0/x86_64-unknown-linux-gnu/rustup-init"
rustup_init_sha256="4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10"

# Extra packages on top of the cloud image's base set (openssh-server, sudo,
# systemd-networkd/resolved, grub, kernel, cloud-guest-utils/growpart, … are
# already present in the cloud image).
packages=(vim tmux htop build-essential git usbutils pciutils nload nano gnupg bc mtr zip unzip wget gpg ca-certificates dbus)

puppet_daemon_args='--transport auto_discover'
serial_consoles=(ttyS0)
