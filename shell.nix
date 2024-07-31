# Licensed under the Apache License, Version 2.0 or the MIT License.
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Shell expression for the Nix package manager
#
# To use:
#
#  $ nix-shell
#
{pkgs ? import <nixpkgs> {}}:
with builtins; let
  rust_overlay = import "${pkgs.fetchFromGitHub {
    owner = "nix-community";
    repo = "fenix";
    rev = "1a92c6d75963fd594116913c23041da48ed9e020";
    sha256 = "sha256-L3vZfifHmog7sJvzXk8qiKISkpyltb+GaThqMJ7PU9Y=";
  }}/overlay.nix";

  nixpkgs = import <nixpkgs> {overlays = [rust_overlay];};

  # Get a custom cross-compile capable Rust install of a specific channel and
  # build.
  rustBuild = nixpkgs.fenix.fromToolchainFile {file = ./rust-toolchain.toml;};
in
  pkgs.mkShell {
    name = "treadmill-dev";

    buildInputs = with pkgs; [
      # --- Toolchains ---
      rustBuild
      # --- CI support packages ---
      qemu
    ];
  }
