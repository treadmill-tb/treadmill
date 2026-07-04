{
  description = "A basic flake with a shell";
  inputs.nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05-small";
  inputs.systems.url = "github:nix-systems/default";
  inputs.flake-utils = {
    url = "github:numtide/flake-utils";
    inputs.systems.follows = "systems";
  };

  outputs =
    { nixpkgs, nixpkgs-unstable, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        unstable = nixpkgs-unstable.legacyPackages.${system};
      in
        {
          devShells.default = pkgs.mkShell { packages = [
            pkgs.bashInteractive
            pkgs.rustup
            pkgs.wasm-tools

            # Need unstabel channel for now because 15.1 won't start
            # `serve`
            unstable.fastly
          ]; };
      }
    );
}
