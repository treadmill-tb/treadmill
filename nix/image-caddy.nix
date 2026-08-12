_: {
  perSystem =
    { pkgs, ... }:
    let
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;

      mkImageCaddy =
        crossPkgs:
        import ./pkgs/job-gateway-caddy.nix {
          pkgs = crossPkgs;
          static = true;
        };
    in
    {
      packages = lib.optionalAttrs isLinux {
        tml-caddy-static-x86_64 = mkImageCaddy pkgs.pkgsCross.musl64.pkgsStatic;
        tml-caddy-static-aarch64 = mkImageCaddy pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic;
      };
    };
}
