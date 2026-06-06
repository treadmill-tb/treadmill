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
          group = cmn.groups.cli;
          bin = "tml";
        };

        swx = cmn.mkBin {
          group = cmn.groups.switchboard;
          bin = "swx";
        };

        treadmill-qemu-supervisor = cmn.mkBin {
          group = cmn.groups.supervisors;
          bin = "treadmill-qemu-supervisor";
        };

        treadmill-nbd-netboot-supervisor = cmn.mkBin {
          group = cmn.groups.supervisors;
          bin = "treadmill-nbd-netboot-supervisor";
        };
      }
      // lib.optionalAttrs isLinux {
        tml-puppet = cmn.mkBin {
          group = cmn.groups.puppet;
          bin = "tml-puppet";
        };
      };
    };
}
