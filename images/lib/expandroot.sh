#!/bin/bash
set -euxo pipefail

# Root device (e.g. mmcblk0p2 / sda1 / nvme0n1p2), without the /dev prefix.
# The sed expression exits non-zero (`$q1`, aborting the script under `set -e`)
# if the root source is not a plain /dev/<name> block device — e.g. a btrfs
# subvolume or ZFS dataset, which we cannot grow this way.
# https://stackoverflow.com/a/15966279
#
# shellcheck disable=SC2016
ROOTDEV="$(findmnt -n -o SOURCE / | sed -E '/^\/dev\/([[:alnum:]]+)$/{s//\1/;b};$q1')"
echo "Identified root file system device as /dev/$ROOTDEV" >&2

# We support the root filesystem residing on a partition of a disk with a
# partition table, or directly on the disk. sysfs tells us which: a partition
# has a `partition` attribute naming its number.
ROOTDEV_SYSFS="/sys/class/block/$ROOTDEV"
if [ -f "$ROOTDEV_SYSFS/partition" ]; then
	ROOTDEV_PARTNUM="$(cat "$ROOTDEV_SYSFS/partition")"
	ROOTDEV_BASEDEV_SYSFS="$(readlink -f "$ROOTDEV_SYSFS/..")"
	ROOTDEV_BASEDEV="$(basename "$ROOTDEV_BASEDEV_SYSFS")"
	echo "Root filesystem device is partition $ROOTDEV_PARTNUM of device /dev/$ROOTDEV_BASEDEV" >&2

	# The parent of a partition must be a whole disk, not another partition
	# (we have no way to grow a nested layout).
	if [ -f "$ROOTDEV_BASEDEV_SYSFS/partition" ]; then
		echo "Device $ROOTDEV_BASEDEV is itself a partition. This is not supported." >&2
		exit 1
	fi

	echo "Expanding partition $ROOTDEV_PARTNUM of device /dev/$ROOTDEV_BASEDEV..." >&2
	if command -v growpart >/dev/null; then
		# Grows the partition to fill the disk; exits non-zero when there is
		# nothing to grow, which is not an error for us.
		growpart -u force "/dev/$ROOTDEV_BASEDEV" "$ROOTDEV_PARTNUM" || true
	elif command -v parted >/dev/null; then
		# -s: never prompt (this runs unattended from a systemd unit).
		parted -s "/dev/$ROOTDEV_BASEDEV" resizepart "$ROOTDEV_PARTNUM" 100%
	else
		echo "Cannot find any supported tool to grow root partition!" >&2
		exit 1
	fi
fi

echo "Resizing root file system on /dev/$ROOTDEV to partition size..." >&2
resize2fs "/dev/$ROOTDEV" # no-op if already at full size

echo "Successfully expanded root disk!" >&2
