#!/usr/bin/env bash
# Download + install the latest GitHub Actions self-hosted runner release into
# /opt/gh-actions-runner.
#
# Run ONCE at first boot (install-gh-actions-runner.service), NOT at image-build
# time: pinning a runner version in the image goes stale within weeks (the
# runner auto-update is disabled for ephemeral/JIT runners), so we always fetch
# the current release at deploy time instead. Ported from the legacy
# images/raspbian-13/install-gh-actions.sh, promoted to images/lib/ so both the
# Ubuntu and Raspberry Pi OS gha-runner overlays share it.
#
# Parameterized via the environment (set in the systemd unit):
#   RUNNER_ARCH   release arch slug: x64 (x86_64) or arm64 (aarch64)
#   RUNNER_OWNER  owner for the unpacked tree (username or uid, e.g. 1000)
#   RUNNER_GROUP  group for the unpacked tree (group name or gid, e.g. 1000)
set -euo pipefail

if [ -e /opt/gh-actions-runner ]; then
	echo "Target directory /opt/gh-actions-runner exists, exiting!" >&2
	exit 0
fi
if [ "$(id -u)" != 0 ]; then
	echo "This script must be run as root!" >&2
	exit 1
fi
: "${RUNNER_ARCH:?must set RUNNER_ARCH (e.g. x64, arm64)}"
: "${RUNNER_OWNER:?must set RUNNER_OWNER (username or uid)}"
: "${RUNNER_GROUP:?must set RUNNER_GROUP (group name or gid)}"

set -x

# Resolve the latest release version from the releases/latest redirect target
# (.../releases/tag/v<version>) — no jq/python dependency in the guest.
latest_url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
	https://github.com/actions/runner/releases/latest)"
version="${latest_url##*/tag/v}"
if [ -z "$version" ] || [ "$version" = "$latest_url" ]; then
	echo "could not determine latest runner version from: $latest_url" >&2
	exit 1
fi
echo "Installing GitHub Actions runner v${version} (${RUNNER_ARCH})" >&2

tarball_url="https://github.com/actions/runner/releases/download/v${version}/actions-runner-linux-${RUNNER_ARCH}-${version}.tar.gz"
curl --fail -L -o /opt/gh-actions-runner.tar.gz "$tarball_url"

mkdir -p /opt/gh-actions-runner
tar -xf /opt/gh-actions-runner.tar.gz -C /opt/gh-actions-runner
rm -f /opt/gh-actions-runner.tar.gz

chown -R "${RUNNER_OWNER}:${RUNNER_GROUP}" /opt/gh-actions-runner
chmod -R u+w /opt/gh-actions-runner
