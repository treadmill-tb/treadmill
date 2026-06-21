# images/raspberrypios-13/manifest.sh — data only, sourced by build-image.sh.
#
# Raspberry Pi OS 13 (trixie), arm64, NBD-netboot. Built from the upstream
# Raspberry Pi OS Lite SD image (libguestfs pipeline; see
# doc/images-libguestfs-build-plan.md). build-image.sh's `sd` path extracts the
# two partitions: the FAT boot partition is edited in place (mtools) and shipped
# as a standalone `boot` layer, the ext4 root partition becomes the lowest
# `root` layer with our provisioning layered on as a qcow2 overlay.
arch="aarch64"
type="sd" # FAT boot partition + ext4 root partition

title="Raspberry Pi OS 13 (NBD)"
version="13"

# Upstream Raspberry Pi OS Lite (arm64), pinned to a dated release for a stable
# URL + checksum (the bare `images/` index is a moving target). The .img.xz is
# decompressed and its partitions extracted by build-image.sh. The sha256 is
# verified on every fetch as a drift guard. No personal mirror.
base_image_url="https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-04-21/2026-04-21-raspios-trixie-arm64-lite.img.xz"
base_image_sha256="4cd31df026fd82243805a326dc0cafd7383f7e3d30c9413e7044d507aae281e2"

# rustup-init (aarch64): installed for the tml user, no default toolchain. Pinned
# to the upstream versioned archive on static.rust-lang.org (stable URL + hash);
# the sha256 is verified on every fetch.
rustup_init_url="https://static.rust-lang.org/rustup/archive/1.29.0/aarch64-unknown-linux-gnu/rustup-init"
rustup_init_sha256="9732d6c5e2a098d3521fca8145d826ae0aaa067ef2385ead08e6feac88fa5792"

# nbd-client is pulled from the live Debian/Raspberry Pi OS archive (apt, like
# the rest of our package installs) rather than a pinned .deb on a personal
# mirror. openssh-server / udev / dbus / systemd-networkd are already present.
packages=(nbd-client)

# The netboot target has no D-Bus auto-discovery: the puppet daemon reaches the
# supervisor over TCP, at the default gateway on port 3859. The `$(ip route …)`
# is left literal in the unit and evaluated at service start (see
# provision-common.sh). Must contain no single quote.
puppet_daemon_args='--transport tcp --tcp-control-socket-addr "$(ip route show 0.0.0.0/0 | cut -d" " -f3 | head -n1):3859"'

serial_consoles=(ttyAMA0 ttyAMA10)

# Kernel cmdline written to the boot FAT's cmdline.txt by build-image.sh: NBD
# root over DHCP, predictable NIC naming off (net.ifnames=0 -> eth0).
nbd_cmdline="console=serial0,115200 ip=dhcp root=/dev/nbd0 rw nbdroot=dhcp,root,nbd0 rootfstype=ext4 fsckfix rootwait net.ifnames=0 loglevel=7"
