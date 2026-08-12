# shellcheck shell=bash source=build-image.sh
#
# Pinned ttyd release, sourced by build-image.sh for the base variant after the
# image manifest (so $arch is set). Upstream's binaries are statically linked, so
# the same ones work on every image; ttyd is not packaged for Debian at all.
# Upstream names its assets by uname -m, as the manifests do.
ttyd_version="1.7.7"

# shellcheck disable=SC2154
case "$arch" in
x86_64)
	export ttyd_sha256="8a217c968aba172e0dbf3f34447218dc015bc4d5e59bf51db2f2cd12b7be4f55"
	;;
aarch64)
	export ttyd_sha256="b38acadd89d1d396a0f5649aa52c539edbad07f4bc7348b27b4f4b7219dd4165"
	;;
*) die "no pinned ttyd release for arch $arch" ;;
esac

export ttyd_url="https://github.com/tsl0922/ttyd/releases/download/${ttyd_version}/ttyd.${arch}"
