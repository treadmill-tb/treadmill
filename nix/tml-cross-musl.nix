{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      ...
    }:
    let
      cmn = import ./lib.nix { inherit inputs system pkgs; };
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;
      inherit (inputs) fenix;

      fenixPkgs = fenix.packages.${system};

      mkStaticTml =
        {
          targetTriple,
          crossPkgs,
        }:
        let
          rust = fenixPkgs.combine [
            fenixPkgs.stable.rustc
            fenixPkgs.stable.cargo
            fenixPkgs.targets.${targetTriple}.stable.rust-std
          ];
          rustPlatform = crossPkgs.pkgsStatic.makeRustPlatform {
            rustc = rust;
            cargo = rust;
          };
        in
        rustPlatform.buildRustPackage {
          pname = "tml";
          version = "0.1.0";

          # Same per-crate fileset as the native crane build: workspace skeleton
          # + cli/, treadmill-rs/.
          # Editing other workspace crates won't invalidate this build.
          src = cmn.binSrcs.tml;
          buildAndTestSubdir = "cli";
          buildNoDefaultFeatures = true;
          buildFeatures = [ "daemon" ];

          cargoLock.lockFile = ../Cargo.lock;

          target = targetTriple;
          doCheck = false;
        };
    in
    {
      packages = lib.optionalAttrs isLinux {
        tml-static-x86_64 = mkStaticTml {
          targetTriple = "x86_64-unknown-linux-musl";
          crossPkgs = pkgs.pkgsCross.musl64;
        };

        tml-static-aarch64 = mkStaticTml {
          targetTriple = "aarch64-unknown-linux-musl";
          crossPkgs = pkgs.pkgsCross.aarch64-multiplatform;
        };
      };
    };
}
