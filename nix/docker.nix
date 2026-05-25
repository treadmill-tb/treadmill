_: {
  perSystem =
    {
      pkgs,
      self',
      ...
    }:
    let
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;
    in
    {
      packages = lib.optionalAttrs isLinux {
        # Streamed layered image for the switchboard server, suitable for
        # `fly deploy --image` after `docker load`.
        #
        # Build with:
        #   nix build .#swx-image
        # Load and push with:
        #   $(nix build .#swx-image --print-out-paths) | docker load
        swx-image = pkgs.dockerTools.streamLayeredImage {
          name = "treadmill-switchboard";
          tag = "latest";

          contents = [
            pkgs.cacert
            pkgs.tzdata
            (pkgs.buildEnv {
              name = "image-root";
              paths = [
                self'.packages.swx
                pkgs.dockerTools.usrBinEnv
                pkgs.dockerTools.binSh
                pkgs.dockerTools.caCertificates
                pkgs.dockerTools.fakeNss
              ];
              pathsToLink = [
                "/bin"
                "/etc"
                "/share"
              ];
            })
          ];

          config = {
            Entrypoint = [
              "/bin/swx"
              "serve"
            ];
            Env = [
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "TZDIR=/share/zoneinfo"
            ];
            WorkingDir = "/";
          };
        };
      };
    };
}
