# shellcheck shell=bash source=build-image.sh
#
# Pinned code-server release, sourced by build-image.sh for the `webide` variant
# after the image manifest (so $arch is set). The .deb rather than the standalone
# tarball: it declares no dependencies, so the overlay installs it offline.
code_server_version="4.132.0"

# shellcheck disable=SC2154
case "$arch" in
x86_64)
	code_server_platform="amd64"
	export code_server_sha256="18e0e69920ab23b725cb219fb42bc045a908421448cf496a3124314e1a02bcf1"
	;;
aarch64)
	code_server_platform="arm64"
	export code_server_sha256="a7f76980a44bce06490db5fb8ee50b2b94674b3940f04899c818b3ef4058cfc2"
	;;
*) die "no pinned code-server release for arch $arch" ;;
esac

export code_server_url="https://github.com/coder/code-server/releases/download/v${code_server_version}/code-server_${code_server_version}_${code_server_platform}.deb"
