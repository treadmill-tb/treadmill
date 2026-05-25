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
    in
    {
      packages = rec {
        default = tml;

        tml = cmn.mkBin {
          crate = "treadmill-cli";
          bin = "tml";
        };

        swx = cmn.mkBin {
          crate = "treadmill-switchboard";
          bin = "swx";
        };

        treadmill-qemu-supervisor = cmn.mkBin {
          crate = "treadmill-qemu-supervisor";
          bin = "treadmill-qemu-supervisor";
        };

        treadmill-nbd-netboot-supervisor = cmn.mkBin {
          crate = "treadmill-nbd-netboot-supervisor";
          bin = "treadmill-nbd-netboot-supervisor";
        };

        treadmill-mock-supervisor = cmn.mkBin {
          crate = "treadmill-mock-supervisor";
          bin = "treadmill-mock-supervisor";
        };
      }
      // lib.optionalAttrs isLinux {
        tml-puppet = cmn.mkBin {
          crate = "treadmill-puppet";
          bin = "tml-puppet";
        };
      };
    };
}
