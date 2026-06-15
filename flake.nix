{
  description = "Treadmill -- A Distributed Hardware Testbed";

  nixConfig = {
    extra-substituters = [ "https://treadmill-tb.cachix.org" ];
    extra-trusted-public-keys = [
      "treadmill-tb.cachix.org-1:ivmCI8wWEGxVE0+599Bwd5wynPFV+Tw+mW6RHzlqxuE="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [
        inputs.treefmt-nix.flakeModule
        ./nix/treefmt.nix
        ./nix/packages.nix
        ./nix/apps.nix
        ./nix/puppet-cross-musl.nix
        ./nix/tiny-efi.nix
        ./nix/images.nix
        ./nix/devshells.nix
        ./nix/checks.nix
        ./nix/docker.nix
      ];
    };
}
