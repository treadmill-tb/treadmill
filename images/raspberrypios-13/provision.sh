#!/bin/sh
# Raspberry Pi OS-specific in-guest provisioning (root delta only; the boot FAT
# partition is edited host-side in build-image.sh).
#
# Runs under `virt-customize --run` AFTER images/lib/provision-common.sh, on the
# extracted ext4 root partition. The guest kernel is NEVER booted. See
# doc/images-libguestfs-build-plan.md §7. The legacy DTB-merge /
# qemu-system-aarch64 customize-boot / sysrq / 7z surgery / /boot/firmware-mock
# machinery is gone: libguestfs mounts the root directly, and at runtime
# /boot/firmware is the real TFTP FAT.
set -eu

# --- drop the stock pi user (we run as tml, created in provision-common) ---
userdel -r pi 2>/dev/null || true

# --- extra device groups for the tml user (RPi peripherals) ----------------
# plugdev + tty are added in provision-common; dialout (serial) and gpio are
# RPi-only and exist on Raspberry Pi OS.
usermod -a -G dialout tml
usermod -a -G gpio tml

# --- NBD root fstab --------------------------------------------------------
# Root is the NBD device (mounted by the initramfs from the cmdline.txt nbdroot=
# arg); /boot/firmware is the real TFTP FAT at runtime and is not mounted here.
cat >/etc/fstab <<'FSTAB'
proc /proc proc defaults 0 0
/dev/nbd0 / ext4 defaults,noatime,nodiratime 0 1
FSTAB

# --- mask SD-card / firmware-only units incompatible with NBD netboot ------
# These fail or are meaningless without a real SD card / writable firmware FAT.
for unit in \
	dphys-swapfile.service \
	rpi-eeprom-update.service \
	userconfig.service \
	systemd-hostnamed.service \
	systemd-hostnamed.socket \
	systemd-logind.service \
	rpi-resize.service \
	systemd-growfs-root.service \
	rpi-resize-swap-file.service \
	sshswitch.service \
	cloud-init.service \
	cloud-init-local.service \
	cloud-init-network.service \
	cloud-final.service \
	cloud-init.target; do
	ln -snf /dev/null "/etc/systemd/system/$unit"
done

# --- ssh: enabled directly (sshswitch, which keys off the boot-FAT ssh.txt,
# --- is masked above); provision-common owns unique host keys per deployment.
systemctl enable ssh.service

# nbd-client itself is installed from the live archive via the manifest
# `packages` list (build-image.sh `--install nbd-client`).
