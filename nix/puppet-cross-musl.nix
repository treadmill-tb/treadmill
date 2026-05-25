{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      ...
    }:
    let
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;
      inherit (inputs) fenix;

      fenixPkgs = fenix.packages.${system};

      mkStaticPuppet =
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
          pname = "tml-puppet";
          version = "0.1.0";

          src = lib.cleanSource ../.;
          buildAndTestSubdir = "puppet";

          cargoLock.lockFile = ../Cargo.lock;

          target = targetTriple;
          doCheck = false;
        };
    in
    {
      packages = lib.optionalAttrs isLinux {
        tml-puppet-static-x86_64 = mkStaticPuppet {
          targetTriple = "x86_64-unknown-linux-musl";
          crossPkgs = pkgs.pkgsCross.musl64;
        };

        tml-puppet-static-aarch64 = mkStaticPuppet {
          targetTriple = "aarch64-unknown-linux-musl";
          crossPkgs = pkgs.pkgsCross.aarch64-multiplatform;
        };
      };
    };
}
