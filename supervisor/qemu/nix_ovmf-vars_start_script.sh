#! /usr/bin/env bash

set -e -x

OVMF_PATH="$(nix eval --extra-experimental-features nix-command --impure --raw --expr 'with import <nixpkgs> {}; OVMF.fd')"

cp "$OVMF_PATH/FV/OVMF_CODE.fd" "$TML_JOB_WORKDIR/OVMF_CODE.fd"
cp "$OVMF_PATH/FV/OVMF_VARS.fd" "$TML_JOB_WORKDIR/OVMF_VARS.fd"
chmod u+w "$TML_JOB_WORKDIR/OVMF_VARS.fd"
