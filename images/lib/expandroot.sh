#!/bin/bash
set -e -x

# root device (e.g., mmcblk0p2), without /dev prefix
# the sed expression errors if no [[:alnum:]] match can be made on the
# device name (in case of trailing snapshots on btrfs, or ZFS, etc.)
# https://stackoverflow.com/a/15966279
ROOTDEV="$(findmnt -n -o SOURCE / | sed -E '/^\/dev\/([[:alnum:]]+)$/{s//\1/;b};$q1')"
echo "Identified root file system device as /dev/$ROOTDEV" >&2

# We support the root file system residing on a partition on a disk with a
# partition table, or on the disk directly. We use the sysfs to identify if
# we're on a partition.
ROOTDEV_SYSFS="/sys/class/block/$ROOTDEV"
if [ -f "$ROOTDEV_SYSFS/partition" ]; then
  ROOTDEV_PARTNUM="$(cat "$ROOTDEV_SYSFS/partition")"
  ROOTDEV_BASEDEV_SYSFS="$(readlink -f "/sys/class/block/$ROOTDEV/..")"
  ROOTDEV_BASEDEV="$(basename "$ROOTDEV_BASEDEV_SYSFS")"
  echo "Root filesystem device is partition $ROOTDEV_PARTNUM of device /dev/$ROOTDEV_BASEDEV" >&2

  # Make sure that the base device is not itself a partition:
  if [ -f "ROOTDEV_BASEDEV_SYSFS/parition" ]; then
    echo "Device $ROOTDEV_BASEDEV is itself a partition. This is not supported." >&2
    exit 1
  fi

  echo "Expanding partition $ROOTDEV_PARTNUM of device /dev/$ROOTDEV_BASEDEV..." >&2
  if which growpart >/dev/null; then
    growpart -u force "/dev/$ROOTDEV_BASEDEV" "$ROOTDEV_PARTNUM" || true # this fails if part can't be grown
  elif which parted >/dev/null; then
    parted "/dev/$ROOTDEV_BASEDEV" resizepart "$ROOTDEV_PARTNUM" "100%"
  else
    echo "Cannot find any supported tool to grow root partition!" >&2
    exit 1
  fi
fi

echo "Resizing root file system on /dev/$ROOTDEV to partition size..." >&2
resize2fs "/dev/$ROOTDEV" # doesn't fail if nothing to do

echo "Successfully expanded root disk!" >&2

