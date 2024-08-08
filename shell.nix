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

  rustBuild = nixpkgs.fenix.fromToolchainFile {file = ./rust-toolchain.toml;};
in
  pkgs.mkShell {
    name = "treadmill-dev";

    buildInputs = with pkgs; [
      # --- Toolchains ---
      rustBuild
      openssl
      pkg-config
      # --- CI support packages ---
      # qemu
    ];
    shellHook = ''
      export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
      export OPENSSL_DIR="${pkgs.openssl.dev}"
      export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
      export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"
    '';
  }
