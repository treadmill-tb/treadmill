#!/usr/bin/env bash
# Treadmill image build driver (libguestfs pipeline).
#
# Fetches a verified upstream cloud/SD image, layers our provisioning on top as a
# qcow2 differential overlay (`virt-customize`, no guest boot), finalizes the
# delta, then assembles + validates an OCI image layout via `image-util`.
#
# See doc/images-libguestfs-build-plan.md §5. Two image types and two variants:
#
#   type=disk  (manifest)  the cloud image IS a whole-disk qcow2 (Ubuntu): it is
#                          the verbatim lowest root layer; our delta overlays it.
#   type=sd    (manifest)  an SD card image (Raspberry Pi OS): the FAT boot
#                          partition is edited host-side and shipped as a
#                          standalone `boot` layer, the ext4 root partition
#                          becomes the lowest `root` layer.
#
#   --variant base         the base image (root layers: layer0 + provisioning
#                          delta).
#   --variant gha-runner   an overlay (root layers: layer0 + base delta + runner
#                          delta) adding the GitHub Actions runner units on top
#                          of the byte-identical base delta. Requires
#                          --base-delta (`<base-out>.build/delta.qcow2`); for
#                          type=sd it also requires --boot-fat (the base build's
#                          byte-identical boot blob, shared verbatim).
#
# Usage:
#   images/lib/build-image.sh <name> --puppet <path> --image-util <path> \
#       [--variant base] -o <out-layout-dir>
#   images/lib/build-image.sh <name> --puppet <path> --image-util <path> \
#       --variant gha-runner --base-delta <base-out>.build/delta.qcow2 \
#       [--boot-fat <base-out>.build/boot.fat] -o <out-layout-dir>
#
# The build's intermediate blobs (layer0, delta, boot.fat) are kept in
# `<out>.build/` so a later overlay variant can reuse the byte-identical ones.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
images_dir="$(dirname "$here")"

die() {
	echo "build-image: $*" >&2
	exit 1
}

# Finalize a provisioning delta in place: trim freed blocks, detach the backing
# pointer (so it is a standalone differential blob), and compress it.
finalize_delta() {
	local d="$1"
	virt-customize -a "$d" --run-command 'fstrim -av || true'
	qemu-img rebase -u -b "" -F qcow2 "$d"
	qemu-img convert -c -O qcow2 "$d" "$d.c" && mv "$d.c" "$d"
}

# Grow a root delta + its filesystem by $build_grow so apt has room. The shipped
# virtual size grows accordingly — harmless: expandroot grows root to the host
# disk at deploy, and the compressed blob only stores used clusters.
grow_root_delta() {
	local d="$1" t="$2"
	qemu-img resize "$d" "+${build_grow:-4G}" >/dev/null
	if [ "$t" = disk ]; then
		# Whole-disk image: extend the (physically last) root partition into the
		# new space, then the filesystem. Root is partition 1 on the Ubuntu cloud
		# image; the table is GPT.
		local grown_bytes
		grown_bytes="$(qemu-img info "$d" | sed -n 's/.*(\([0-9][0-9]*\) bytes).*/\1/p' | head -n1)"
		guestfish --rw -a "$d" <<-GFS
			run
			part-expand-gpt /dev/sda
			part-resize /dev/sda 1 $((grown_bytes / 512 - 34))
			resize2fs /dev/sda1
		GFS
	else
		# Bare ext4 root partition (sd): the whole device is the filesystem.
		guestfish --rw -a "$d" <<-GFS
			run
			e2fsck-f /dev/sda
			resize2fs /dev/sda
		GFS
	fi
}

name=""
puppet=""
image_util=""
variant="base"
base_delta=""
boot_fat_in=""
out=""

# The image name is the first positional argument.
if [ $# -gt 0 ] && [[ "$1" != -* ]]; then
	name="$1"
	shift
fi
while [ $# -gt 0 ]; do
	case "$1" in
	--puppet)
		puppet="$2"
		shift 2
		;;
	--image-util)
		image_util="$2"
		shift 2
		;;
	--variant)
		variant="$2"
		shift 2
		;;
	--base-delta)
		base_delta="$2"
		shift 2
		;;
	--boot-fat)
		boot_fat_in="$2"
		shift 2
		;;
	-o | --out)
		out="$2"
		shift 2
		;;
	*) die "unknown argument: $1" ;;
	esac
done

[ -n "$name" ] || die "missing <name> (e.g. ubuntu-server-2604)"
[ -n "$puppet" ] || die "missing --puppet <path>"
[ -n "$image_util" ] || die "missing --image-util <path>"
[ -n "$out" ] || die "missing -o <out-layout-dir>"
[ -x "$puppet" ] || die "puppet binary not executable: $puppet"
[ -x "$image_util" ] || die "image-util binary not executable: $image_util"

case "$variant" in
base)
	[ -z "$base_delta" ] || die "--base-delta is only valid with --variant gha-runner"
	[ -z "$boot_fat_in" ] || die "--boot-fat is only valid with --variant gha-runner"
	provision="$images_dir/$name/provision.sh"
	;;
gha-runner)
	[ -n "$base_delta" ] || die "--variant gha-runner requires --base-delta <path>"
	[ -f "$base_delta" ] || die "base delta not found: $base_delta"
	provision="$images_dir/$name/gha-runner/provision.sh"
	;;
*) die "unknown --variant: $variant (expected: base, gha-runner)" ;;
esac

manifest="$images_dir/$name/manifest.sh"
[ -f "$manifest" ] || die "no manifest for image $name: $manifest"
[ -f "$provision" ] || die "missing provision script: $provision"

# Absolute paths: virt-customize and image-util run from a temp cwd / appliance.
mkdir -p "$out"
out="$(cd "$out" && pwd)"
puppet="$(cd "$(dirname "$puppet")" && pwd)/$(basename "$puppet")"
image_util="$(cd "$(dirname "$image_util")" && pwd)/$(basename "$image_util")"
if [ -n "$base_delta" ]; then
	base_delta="$(cd "$(dirname "$base_delta")" && pwd)/$(basename "$base_delta")"
fi
if [ -n "$boot_fat_in" ]; then
	boot_fat_in="$(cd "$(dirname "$boot_fat_in")" && pwd)/$(basename "$boot_fat_in")"
fi

# Variables sourced from the manifest below. Declared here and then checked to
# be populated to silence shellcheck warnings and prevent accidental drift:
arch="" type="" title="" version="" base_image_url="" base_image_sha256=""
rustup_init_url="" rustup_init_sha256="" puppet_daemon_args="" nbd_cmdline=""
declare -a packages=()
declare -a serial_consoles=()

# shellcheck source=/dev/null
source "$manifest"

# Enforce manifest-exported string variables are set and non-empty:
: "${arch:?arch missing from $manifest}"
: "${type:?type missing from $manifest}"
: "${title:?title missing from $manifest}"
: "${version:?version missing from $manifest}"
: "${base_image_url:?base_image_url missing from $manifest}"
: "${base_image_sha256:?base_image_sha256 missing from $manifest}"
: "${rustup_init_url:?rustup_init_url missing from $manifest}"
: "${rustup_init_sha256:?rustup_init_sha256 missing from $manifest}"
: "${puppet_daemon_args:?puppet_daemon_args missing from $manifest}"

# Enforce manifest-provided array variables are populated.
if (( ${#packages[@]} == 0 )); then
	echo "Error: packages array is empty in $manifest" >&2
	exit 1
fi
if (( ${#serial_consoles[@]} == 0 )); then
	echo "Error: serial_consoles array is empty in $manifest" >&2
	exit 1
fi

# Enforce manifest-provided conditional variables are populated.
if [[ "$type" == "sd" ]]; then
	: "${nbd_cmdline:?nbd_cmdline missing from $manifest for sd type}"
fi

case "$type" in
disk | sd) ;;
*) die "unknown image type '$type' (expected: disk, sd)" ;;
esac

# An sd gha-runner overlay reuses the base build's byte-identical boot blob.
if [ "$variant" = gha-runner ]; then
	if [ "$type" = sd ]; then
		[ -n "$boot_fat_in" ] || die "sd gha-runner requires --boot-fat <path>"
		[ -f "$boot_fat_in" ] || die "boot fat not found: $boot_fat_in"
	elif [ -n "$boot_fat_in" ]; then
		die "--boot-fat is only used for sd images"
	fi
fi

# The runner variant ships as a distinct image (extra root layer + a suffixed
# name/title); the base variant ships the manifest's name/title verbatim.
if [ "$variant" = gha-runner ]; then
	image_name="$name-gha-runner"
	image_title="$title with GitHub Actions Runner"
else
	image_name="$name"
	image_title="$title"
fi

# Intermediate blobs live alongside the layout; kept for overlay reuse.
work_dir="${out%/}.build"
rm -rf "$work_dir"
mkdir -p "$work_dir"
work_dir="$(cd "$work_dir" && pwd)"
echo "build-image: $image_name ($variant, $type) -> $out   (work dir: $work_dir)" >&2

# --- 1. Fetch + verify the upstream base ----------------------------------
# The sha256 is the drift guard (verified on every fetch).
base_dl="$work_dir/base.dl"
echo "build-image: fetching base image..." >&2
curl -fL "$base_image_url" -o "$base_dl" || die "failed to fetch base image"
echo "${base_image_sha256}  ${base_dl}" | sha256sum -c - ||
	die "base image checksum mismatch"

# --- 2. Derive the lowest root layer (layer0) and, for sd, the boot blob ---
# layer0 must be byte-reproducible (it is the dedupe blob shared across the base
# and overlay layouts): for disk it is the verbatim upstream qcow2; for sd it is
# the extracted ext4 root partition converted to qcow2 (a deterministic dump of
# the same upstream partition).
layer0="$work_dir/layer0.qcow2"
boot_fat=""
if [ "$type" = disk ]; then
	mv "$base_dl" "$layer0"
else
	# Decompress the SD image, then extract its two partitions with guestfish
	# (no guest boot): p1 = FAT boot, p2 = ext4 root.
	sd_img="$work_dir/sd.img"
	xz -dc "$base_dl" >"$sd_img" || die "failed to decompress sd image"
	rm -f "$base_dl"
	root_raw="$work_dir/root.raw"
	extracted_fat="$work_dir/boot.fat"
	guestfish --ro -a "$sd_img" <<-GFS
		run
		download /dev/sda1 $extracted_fat
		download /dev/sda2 $root_raw
	GFS
	qemu-img convert -f raw -O qcow2 "$root_raw" "$layer0"
	rm -f "$sd_img" "$root_raw"

	if [ "$variant" = base ]; then
		# Edit the boot FAT in place (no overlay mechanism for boot): point
		# cmdline.txt at the NBD root and drop an empty ssh.txt. mtools writes the
		# raw FAT directly.
		printf '%s\n' "$nbd_cmdline" | mcopy -i "$extracted_fat" -o - ::cmdline.txt
		: | mcopy -i "$extracted_fat" -o - ::ssh.txt
		boot_fat="$extracted_fat"
	else
		# The overlay shares the base build's byte-identical boot blob verbatim
		# (mtools edits are not bit-reproducible), so discard the re-extracted one.
		rm -f "$extracted_fat"
		boot_fat="$boot_fat_in"
	fi
fi

# --- 3. Build the provisioning delta --------------------------------------
delta="$work_dir/delta.qcow2"
if [ "$variant" = base ]; then
	# rustup-init (fetched + verified, copied into the guest).
	rustup_init="$work_dir/rustup-init"
	curl -fL "$rustup_init_url" -o "$rustup_init" || die "failed to fetch rustup-init"
	echo "${rustup_init_sha256}  ${rustup_init}" | sha256sum -c - ||
		die "rustup-init checksum mismatch"
	chmod +x "$rustup_init"

	# Manifest-derived values for the in-guest provision scripts. virt-customize
	# --run does not forward the host environment, so hand them across in a file
	# the scripts source. serial_consoles is flattened to a space-separated
	# string (no bash arrays in the guest shell).
	prov_env="$work_dir/provision.env"
	{
		printf "puppet_daemon_args='%s'\n" "$puppet_daemon_args"
		printf "serial_consoles='%s'\n" "${serial_consoles[*]}"
	} >"$prov_env"

	# Customization delta = qcow2 overlay backed by the verbatim base layer.
	qemu-img create -f qcow2 -b "$layer0" -F qcow2 "$delta" >/dev/null
	grow_root_delta "$delta" "$type"

	# --install <pkg,pkg,...> only when the manifest lists packages.
	install_args=()
	if [ "${#packages[@]}" -gt 0 ]; then
		install_args=(--install "$(
			IFS=,
			echo "${packages[*]}"
		)")
	fi

	virt-customize -a "$delta" \
		--network --memsize 2048 --smp 2 \
		--copy-in "$puppet":/usr/local/bin \
		--copy-in "$rustup_init":/opt \
		--copy-in "$images_dir/lib/expandroot.sh":/opt \
		--copy-in "$prov_env":/tmp \
		"${install_args[@]}" \
		--run "$images_dir/lib/provision-common.sh" \
		--run "$provision"
else
	# gha-runner overlay: a root layer over the base delta. The OCI chain backs
	# onto the byte-identical base delta (--base-delta) so it dedupes with the
	# base image's head. To CUSTOMIZE on top we need a readable chain, so work on
	# a COPY whose backing is re-pointed at layer0 (rebase -u only rewrites the
	# header; the data clusters — hence the digest of the blob we ship — are
	# untouched). No disk grow: the base delta already carries the grown fs.
	base_delta_work="$work_dir/base-delta.work.qcow2"
	cp "$base_delta" "$base_delta_work"
	qemu-img rebase -u -b "$layer0" -F qcow2 "$base_delta_work"

	qemu-img create -f qcow2 -b "$base_delta_work" -F qcow2 "$delta" >/dev/null

	# No --network / --install / rustup: the overlay only writes the runner units
	# (the runner itself is downloaded at first boot, see
	# images/lib/install-gh-actions-runner.sh).
	virt-customize -a "$delta" \
		--memsize 2048 --smp 2 \
		--copy-in "$images_dir/lib/install-gh-actions-runner.sh":/opt \
		--run "$provision"
fi

# --- 4. Finalize: trim, detach backing, compress --------------------------
finalize_delta "$delta"

# --- 5. Assemble + validate the OCI layout --------------------------------
# Layer order (significant — it defines the backing chain): the standalone boot
# blob first (sd only), then the root chain (verbatim layer0, [base delta for
# overlays], head delta).
layer_args=()
[ "$type" = sd ] && layer_args+=(--layer "boot=$boot_fat")
layer_args+=(--layer "root=$layer0")
[ "$variant" = gha-runner ] && layer_args+=(--layer "root=$base_delta")
layer_args+=(--layer "root=$delta")

root_layers=2
[ "$variant" = gha-runner ] && root_layers=3
boot_layers=0
[ "$type" = sd ] && boot_layers=1

"$image_util" assemble \
	--name "$image_name" \
	--title "$image_title" \
	--version "$version" \
	"${layer_args[@]}" \
	-o "$out"

"$image_util" parse "$out" \
	--name "$image_name" \
	--root-layers "$root_layers" \
	--boot-layers "$boot_layers" \
	--title "$image_title"

echo "build-image: $image_name -> $out (OK)" >&2
