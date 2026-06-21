#!/usr/bin/env bash
# Treadmill image build driver (libguestfs pipeline).
#
# Fetches a verified upstream cloud image, layers our provisioning on top as a
# qcow2 differential overlay (`virt-customize`, no guest boot), finalizes the
# delta, then assembles + validates an OCI image layout via `image-util`.
#
# See doc/images-libguestfs-build-plan.md §5. Phase 2 implements the `disk`
# (whole-qcow2) Ubuntu base path; the `sd` (Raspbian) type is Phase 4 and the
# `gha-runner` variant is Phase 3.
#
# Usage:
#   images/lib/build-image.sh <name> --puppet <path> --image-util <path> \
#       [--variant base] -o <out-layout-dir>
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

name=""
puppet=""
image_util=""
variant="base"
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
base) ;;
gha-runner) die "--variant gha-runner is Phase 3 (overlay); not implemented yet" ;;
*) die "unknown --variant: $variant (expected: base)" ;;
esac

manifest="$images_dir/$name/manifest.sh"
[ -f "$manifest" ] || die "no manifest for image $name: $manifest"
provision="$images_dir/$name/provision.sh"
[ -f "$provision" ] || die "missing provision script: $provision"

# Absolute paths: virt-customize and image-util run from a temp cwd / appliance.
mkdir -p "$out"
out="$(cd "$out" && pwd)"
puppet="$(cd "$(dirname "$puppet")" && pwd)/$(basename "$puppet")"
image_util="$(cd "$(dirname "$image_util")" && pwd)/$(basename "$image_util")"

# manifest.sh provides: arch type title version base_image_url base_image_sha256
# base_image_mirror packages[] rustup_init_url rustup_init_sha256
# puppet_daemon_args serial_consoles[].
# shellcheck source=/dev/null
source "$manifest"

[ "$type" = disk ] || die "image type '$type' is not implemented (Phase 2 is disk-only; sd is Phase 4)"

# Intermediate blobs live alongside the layout; kept for overlay reuse.
work_dir="${out%/}.build"
rm -rf "$work_dir"
mkdir -p "$work_dir"
work_dir="$(cd "$work_dir" && pwd)"
echo "build-image: $name ($variant) -> $out   (work dir: $work_dir)" >&2

# --- 1. Fetch + verify the upstream base ----------------------------------
# The amd64 cloud image is already a qcow2; it becomes the lowest, content-
# addressed root layer (the dedupe blob) verbatim.
layer0="$work_dir/layer0.qcow2"
echo "build-image: fetching base image..." >&2
curl -fL "$base_image_url" -o "$layer0" || die "failed to fetch base image"
echo "${base_image_sha256}  ${layer0}" | sha256sum -c - ||
	die "base image checksum mismatch"

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
virt-customize -a "$delta" --run-command 'fstrim -av || true'
qemu-img rebase -u -b "" -F qcow2 "$delta"
qemu-img convert -c -O qcow2 "$delta" "$delta.c" && mv "$delta.c" "$delta"

# --- 6. Assemble + validate the OCI layout --------------------------------
# disk base: two root layers (verbatim upstream + our delta).
"$image_util" assemble \
	--name "$name" \
	--title "$title" \
	--version "$version" \
	--layer "root=$layer0" \
	--layer "root=$delta" \
	-o "$out"

"$image_util" parse "$out" \
	--name "$name" \
	--root-layers 2 \
	--boot-layers 0 \
	--title "$title"

echo "build-image: $name -> $out (OK)" >&2
