#!/usr/bin/env bash
# Treadmill image build driver (libguestfs pipeline).
#
# Fetches a verified upstream cloud image, layers our provisioning on top as a
# qcow2 differential overlay (`virt-customize`, no guest boot), finalizes the
# delta, then assembles + validates an OCI image layout via `image-util`.
#
# See doc/images-libguestfs-build-plan.md §5. Two variants of the `disk`
# (whole-qcow2) Ubuntu path are implemented:
#
#   --variant base        the 2-root-layer base image (layer0 + provisioning
#                         delta).
#   --variant gha-runner  a 3-root-layer overlay (layer0 + base delta + runner
#                         delta) that adds the GitHub Actions runner units on
#                         top of the byte-identical base delta. Requires
#                         --base-delta pointing at the base build's finalized
#                         delta (`<base-out>.build/delta.qcow2`).
#
# The `sd` (Raspberry Pi OS) type is Phase 4.
#
# Usage:
#   images/lib/build-image.sh <name> --puppet <path> --image-util <path> \
#       [--variant base] -o <out-layout-dir>
#   images/lib/build-image.sh <name> --puppet <path> --image-util <path> \
#       --variant gha-runner --base-delta <base-out>.build/delta.qcow2 \
#       -o <out-layout-dir>
#
# The build's intermediate blobs (layer0, delta) are kept in `<out>.build/` so a
# later overlay variant can reuse the byte-identical base delta.
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

name=""
puppet=""
image_util=""
variant="base"
base_delta=""
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

# manifest.sh provides: arch type title version base_image_url base_image_sha256
# base_image_mirror packages[] rustup_init_url rustup_init_sha256
# puppet_daemon_args serial_consoles[].
# shellcheck source=/dev/null
source "$manifest"

[ "$type" = disk ] || die "image type '$type' is not implemented (Phase 2 is disk-only; sd is Phase 4)"

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
echo "build-image: $image_name ($variant) -> $out   (work dir: $work_dir)" >&2

# --- 1. Fetch + verify the upstream base ----------------------------------
# The amd64 cloud image is already a qcow2; it becomes the lowest, content-
# addressed root layer (the dedupe blob) verbatim. The runner overlay refetches
# it (rather than reusing the base build's copy) so this script stays self-
# contained; the sha256 guarantees the resulting layer0 blob is byte-identical
# to the base build's, so the two layouts share it in the registry.
layer0="$work_dir/layer0.qcow2"
echo "build-image: fetching base image..." >&2
curl -fL "$base_image_url" -o "$layer0" || die "failed to fetch base image"
echo "${base_image_sha256}  ${layer0}" | sha256sum -c - ||
	die "base image checksum mismatch"

if [ "$variant" = gha-runner ]; then
	# --- gha-runner overlay: a third root layer over the base delta --------
	# The OCI chain is layer0 -> base-delta -> runner-delta. The base-delta blob
	# in the chain is the byte-identical finalized base delta (passed via
	# --base-delta), so it dedupes with the base image's head blob. To CUSTOMIZE
	# on top of it we need a readable chain, so work on a COPY whose backing is
	# re-pointed at layer0 (rebase -u only rewrites the header; the data clusters
	# — hence the digest of the blob we actually ship — are untouched).
	base_delta_work="$work_dir/base-delta.work.qcow2"
	cp "$base_delta" "$base_delta_work"
	qemu-img rebase -u -b "$layer0" -F qcow2 "$base_delta_work"

	# The runner delta backs onto the (re-chained) base delta. No disk grow: the
	# base delta already carries the grown partition table + filesystem.
	delta="$work_dir/delta.qcow2"
	qemu-img create -f qcow2 -b "$base_delta_work" -F qcow2 "$delta" >/dev/null

	# No --network / --install / rustup: the overlay only writes the runner units
	# (the runner itself is downloaded at first boot, see
	# images/lib/install-gh-actions-runner.sh).
	virt-customize -a "$delta" \
		--memsize 2048 --smp 2 \
		--copy-in "$images_dir/lib/install-gh-actions-runner.sh":/opt \
		--run "$provision"

	finalize_delta "$delta"

	# Assemble + validate: three root layers (verbatim upstream, base delta,
	# runner delta). The base delta is the byte-identical --base-delta blob.
	"$image_util" assemble \
		--name "$image_name" \
		--title "$image_title" \
		--version "$version" \
		--layer "root=$layer0" \
		--layer "root=$base_delta" \
		--layer "root=$delta" \
		-o "$out"

	"$image_util" parse "$out" \
		--name "$image_name" \
		--root-layers 3 \
		--boot-layers 0 \
		--title "$image_title"

	echo "build-image: $image_name -> $out (OK)" >&2
	exit 0
fi

# --- base variant ---------------------------------------------------------

# --- 2. rustup-init (fetched + verified, copied into the guest) -----------
rustup_init="$work_dir/rustup-init"
curl -fL "$rustup_init_url" -o "$rustup_init" || die "failed to fetch rustup-init"
echo "${rustup_init_sha256}  ${rustup_init}" | sha256sum -c - ||
	die "rustup-init checksum mismatch"
chmod +x "$rustup_init"

# --- 3. Manifest-derived values for the in-guest provision scripts ---------
# virt-customize --run does not forward the host environment, so hand the values
# the provision scripts need across in a file they source. serial_consoles is
# flattened to a space-separated string (no bash arrays in the guest shell).
prov_env="$work_dir/provision.env"
{
	printf "puppet_daemon_args='%s'\n" "$puppet_daemon_args"
	printf "serial_consoles='%s'\n" "${serial_consoles[*]}"
} >"$prov_env"

# --- 4. Customization delta = qcow2 overlay backed by the base layer -------
delta="$work_dir/delta.qcow2"
qemu-img create -f qcow2 -b "$layer0" -F qcow2 "$delta" >/dev/null

# The cloud image's root fs is sized to its contents, with no room for our
# packages. Grow the overlay's disk + root partition + filesystem so apt has
# space; the partition-table and fs changes land in the delta (layer0 stays the
# verbatim upstream dedup blob). The shipped virtual size grows accordingly —
# harmless, since expandroot grows root to the host disk at deploy and the
# compressed blob only stores used clusters. Root is partition 1 on the Ubuntu
# cloud image (physically last, so it extends cleanly into the new space).
build_grow="${build_grow:-4G}"
qemu-img resize "$delta" "+$build_grow" >/dev/null
grown_bytes="$(qemu-img info "$delta" | sed -n 's/.*(\([0-9][0-9]*\) bytes).*/\1/p' | head -n1)"
guestfish --rw -a "$delta" <<GFS
run
part-expand-gpt /dev/sda
part-resize /dev/sda 1 $((grown_bytes / 512 - 34))
resize2fs /dev/sda1
GFS

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

# --- 5. Finalize: trim, detach backing, compress --------------------------
finalize_delta "$delta"

# --- 6. Assemble + validate the OCI layout --------------------------------
# disk base: two root layers (verbatim upstream + our delta).
"$image_util" assemble \
	--name "$image_name" \
	--title "$image_title" \
	--version "$version" \
	--layer "root=$layer0" \
	--layer "root=$delta" \
	-o "$out"

"$image_util" parse "$out" \
	--name "$image_name" \
	--root-layers 2 \
	--boot-layers 0 \
	--title "$image_title"

echo "build-image: $image_name -> $out (OK)" >&2
