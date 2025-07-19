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
    rev = "e91719882d0e4366202cc9058eb21df74c0bdb92";
    sha256 = "sha256-rSw0b/ahoZebcp+AZG7uoScB5Q59TYEE5Kx8k0pZp9E=";
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
