#!/usr/bin/env bash

set -e

if [ -e "/opt/gh-actions-runner" ]; then
    echo "Target directory /opt/gh-actions-runner exists, exiting!"
    exit 0
fi

if [ "$(id -u)" != "0" ]; then
    echo "This script must be run as root!" >&2
    exit 1
fi

if [ "$RUNNER_ARCH" == "" ]; then
    echo "Must set RUNNER_ARCH variable" >&2
    exit 1
fi

if [ "$RUNNER_OWNER" == "" ]; then
    echo "Must set RUNNER_OWNER variable (username or uid)" >&2
    exit 1
fi

if [ "$RUNNER_GROUP" == "" ]; then
    echo "Must set RUNNER_GROUP variable (group name or gid)" >&2
    exit 1
fi

set -x

LATEST_RELEASE_VERSION="$(\
  curl --silent --fail "https://api.github.com/repos/actions/runner/releases/latest" \
    | python3 -c 'import json, sys; print(json.loads(sys.stdin.read())["name"].removeprefix("v"))')"
echo "Downloading and installing GitHub actions runner v${LATEST_RELEASE_VERSION} for ${RUNNER_ARCH}" >&2

LATEST_RELEASE_URL="https://github.com/actions/runner/releases/download/v${LATEST_RELEASE_VERSION}/actions-runner-linux-${RUNNER_ARCH}-${LATEST_RELEASE_VERSION}.tar.gz"

echo "Downloading GitHub actions runner release tarball..." >&2
curl --fail -L -o /opt/gh-actions-runner.tar.gz "${LATEST_RELEASE_URL}"

echo "Unpacking release tarball..." >&2
mkdir -p /opt/gh-actions-runner
tar -xvf /opt/gh-actions-runner.tar.gz -C /opt/gh-actions-runner

echo "Changing ownership and permissions..." >&2
chown -R "${RUNNER_OWNER}":"${RUNNER_GROUP}" /opt/gh-actions-runner
chmod -R u+w /opt/gh-actions-runner

