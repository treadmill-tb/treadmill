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
    rev = "c2ac9a5c0d6d16630c3b225b874bd14528d1abe6";
    sha256 = "sha256-1TtFDPhC+ZsrOOtBnry1EZC+WipTTvsOVjIEVugqji8=";
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
