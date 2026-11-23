# `nix run .#build-vm -- <args to tml image build>`
#
# Building an image needs root: it attaches a qcow2 as an NBD block device,
# mounts it, and provisions into it under systemd-nspawn. None of that can be
# done unprivileged, and not for want of trying -- ext4 is not
# `FS_USERNS_MOUNT`, so a user namespace cannot mount it however the namespace
# is set up, which rules out bubblewrap and rootless podman alike. `/dev/nbdN`
# is global kernel state besides.
#
# So rather than ask for root on the developer's machine, get it somewhere it
# is free. This boots a VM that has the toolchain, hands it the build, and
# shuts down. The host needs access to `/dev/kvm` and nothing else.
#
# CI does not need this: a runner is already a VM where the job has root.
{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      self',
      ...
    }:
    let
      # `inputs.nixpkgs.lib` rather than `pkgs.lib`, and `system` rather than
      # `pkgs.stdenv`: what names this module exports must not depend on
      # `pkgs`, or flake-parts has to resolve `pkgs` to work out the shape of
      # the output set and recurses.
      lib = inputs.nixpkgs.lib;

      # The VM is only meaningful where the builder is.
      supported = lib.hasSuffix "-linux" system;

      vm = inputs.nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          "${inputs.nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
          (_: {
            system.stateVersion = lib.versions.majorMinor lib.version;

            virtualisation = {
              memorySize = 4096;
              cores = 4;
              # Scratch for the build: a vendor image plus a grown overlay
              # runs to several GB. Sparse, so this costs what it uses.
              diskSize = 32768;
              graphics = false;
              # The host directory the build reads and writes, mounted at
              # the same path inside so that the arguments need no
              # rewriting. The wrapper sets it.
              sharedDirectories.project = {
                source = ''"$TML_VM_PROJECT"'';
                target = "/tml-vm/project";
                # `writable` defaults off, and a build has to leave a
                # layout behind.
                writable = true;
              };
              # Where the wrapper leaves the command and collects the status.
              sharedDirectories.control = {
                source = ''"$TML_VM_CONTROL"'';
                target = "/tml-vm/control";
                writable = true;
              };
            };

            # `tml image build` shells out to all of these.
            environment.systemPackages = [
              self'.packages.tml
              pkgs.qemu-utils
              pkgs.util-linux
              pkgs.cloud-utils
              pkgs.e2fsprogs
              pkgs.xz
              pkgs.curl
            ];

            # `max_part` has to be set when the module loads, or the kernel
            # reads no partition table and a partitioned image attaches as
            # one opaque device.
            boot.kernelModules = [ "nbd" ];
            boot.extraModprobeConfig = "options nbd max_part=16";

            # Provisioning runs the *guest's* binaries, so a foreign-arch
            # image needs an interpreter registered. Doing it here is half
            # the point of the VM: on the host that is global, root-only
            # state someone has to have set up.
            #
            # Static, because that is what sets binfmt's `F` flag, and `F`
            # is what makes it work inside a container: the kernel opens
            # the interpreter once at registration and holds it. Without
            # it the interpreter is resolved at exec time in the caller's
            # mount namespace -- and provisioning execs the guest's
            # `/bin/sh` inside nspawn, where the Nix store is not mounted.
            boot.binfmt.preferStaticEmulators = true;
            # Its own architecture needs no interpreter, and registering one
            # for it is an error.
            boot.binfmt.emulatedSystems = lib.filter (s: s != system) [
              "aarch64-linux"
              "x86_64-linux"
            ];

            # Provisioning installs packages.
            networking.firewall.enable = false;
            services.resolved.enable = true;

            # The whole reason the VM exists: run the build as root, report
            # what happened, and stop.
            systemd.services.tml-build = {
              description = "Run a Treadmill image build";
              wantedBy = [ "multi-user.target" ];
              after = [
                "network-online.target"
                "systemd-binfmt.service"
              ];
              wants = [ "network-online.target" ];
              serviceConfig = {
                Type = "oneshot";
                StandardOutput = "journal+console";
                StandardError = "journal+console";
              };
              path = [
                self'.packages.tml
                pkgs.qemu-utils
                pkgs.util-linux
                pkgs.cloud-utils
                pkgs.e2fsprogs
                pkgs.xz
                pkgs.curl
                pkgs.systemd
              ];
              script = ''
                status=0
                /tml-vm/control/run.sh || status=$?
                echo "$status" > /tml-vm/control/status || true
                sync
              '';
              # In `postStop` so it runs however the build ended. A failure
              # that skipped the poweroff would leave the VM sitting at a
              # login prompt with nobody watching it.
              postStop = "systemctl poweroff --no-block";
            };

            # Nothing is listening, and a shell on the console is the only
            # way in if a build wedges.
            services.getty.autologinUser = "root";
            users.users.root.initialPassword = "";
          })
        ];
      };

      build-vm = pkgs.writeShellApplication {
        name = "tml-build-vm";
        runtimeInputs = [
          pkgs.coreutils
          pkgs.jq
        ];
        text = ''
          # The directory the build reads and writes. Defaults to the working
          # directory, mounted at the same path inside the VM so that relative
          # and absolute arguments alike mean the same thing on both sides.
          TML_VM_PROJECT="''${TML_VM_PROJECT:-$PWD}"
          TML_VM_PROJECT="$(realpath "$TML_VM_PROJECT")"
          export TML_VM_PROJECT

          if [ ! -w /dev/kvm ]; then
            echo "tml-build-vm: /dev/kvm is not writable; the VM needs it" >&2
            echo "hint: add yourself to the kvm group, or run the build as root" >&2
            exit 1
          fi

          TML_VM_CONTROL="$(mktemp -d)"
          export TML_VM_CONTROL
          trap 'rm -rf "$TML_VM_CONTROL"' EXIT

          # The build, as the VM will run it. Arguments are quoted here rather
          # than passed on the kernel command line, which mangles spaces.
          {
            echo '#!/bin/sh'
            echo 'set -eu'
            echo 'cd /tml-vm/project'
            # Pass the provenance through: the image records what the build
            # was told, and the VM has no git checkout to work it out from.
            for var in TML_IMAGE_VERSION TML_IMAGE_CREATED TML_IMAGE_REVISION \
                       TML_IMAGE_DOCUMENTATION TML_REGISTRY_USERNAME TML_REGISTRY_PASSWORD; do
              if [ -n "''${!var:-}" ]; then
                printf 'export %s=%q\n' "$var" "''${!var}"
              fi
            done
            printf 'exec tml image build'
            for arg in "$@"; do printf ' %q' "$arg"; done
            printf '\n'
          } > "$TML_VM_CONTROL/run.sh"
          chmod 0755 "$TML_VM_CONTROL/run.sh"

          # The root disk is scratch: a fresh one per run, discarded after, so
          # a wedged build cannot poison the next one.
          NIX_DISK_IMAGE="$TML_VM_CONTROL/root.qcow2"
          export NIX_DISK_IMAGE

          # No window, and the console on stdio so the build's output is just
          # this command's output.
          QEMU_OPTS="''${QEMU_OPTS:-} -nographic"
          export QEMU_OPTS

          ${vm.config.system.build.vm}/bin/run-${vm.config.networking.hostName}-vm

          status="$(cat "$TML_VM_CONTROL/status" 2>/dev/null || echo 1)"
          if [ "$status" != 0 ]; then
            echo "tml-build-vm: the build failed inside the VM (status $status)" >&2
          fi
          exit "$status"
        '';
      };
    in
    lib.optionalAttrs supported {
      # Exposed so it can be built and inspected without booting anything.
      packages.build-vm = build-vm;

      apps.build-vm = {
        type = "app";
        program = "${build-vm}/bin/tml-build-vm";
        meta.description = "Build a Treadmill image in a VM, so the host needs no root";
      };
    };
}
