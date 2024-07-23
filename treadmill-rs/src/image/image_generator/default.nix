{pkgs ? import <nixpkgs> {}}:
pkgs.callPackage ./ubuntu-image.nix {}
