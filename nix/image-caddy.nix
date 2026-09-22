_: {
  perSystem =
    { pkgs, ... }:
    let
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;

      mkImageCaddy =
        system:
        import ./pkgs/job-gateway-caddy.nix {
          inherit pkgs;
          staticFor = lib.systems.elaborate system;
        };
    in
    {
      packages = lib.optionalAttrs isLinux {
        tml-caddy-static-x86_64 = mkImageCaddy "x86_64-linux";
        tml-caddy-static-aarch64 = mkImageCaddy "aarch64-linux";
      };
    };
}
