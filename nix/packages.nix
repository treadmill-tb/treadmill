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
    in
    {
      packages = rec {
        default = tml;

        # Vendored OCI registry used by the image-migration store daemon and the
        # registry-backed tests (see nix/pkgs/zot.nix).
        inherit (cmn) zot;

        # Caddy carrying a JWT-verification plugin, run by the dev stack as the
        # job service gateway (see nix/pkgs/job-gateway-caddy.nix).
        inherit (cmn) job-gateway-caddy;

        # Serve TFTP from a FAT file system off of an NBD export.
        inherit (cmn) nbdfatftpd;

        tml = cmn.mkBin {
          bin = "tml";
          features = "treadmill-cli/user";
        };

        swx = cmn.mkBin { bin = "swx"; };

        treadmill-qemu-supervisor = cmn.mkBin {
          bin = "treadmill-qemu-supervisor";
          # The OCI store execs skopeo to copy images into the local Zot.
          runtimePath = [ pkgs.skopeo ];
        };

        image-util = cmn.mkBin { bin = "image-util"; };

        # Runs qemu-storage-daemon, qemu-img and nbdfatftpd per job, and execs
        # skopeo to copy images into the local Zot.
        treadmill-nbd-netboot-supervisor = cmn.mkBin {
          bin = "treadmill-nbd-netboot-supervisor";
          runtimePath = [
            pkgs.skopeo
            pkgs.qemu-utils
            cmn.nbdfatftpd
          ];
        };
      };
    };
}
