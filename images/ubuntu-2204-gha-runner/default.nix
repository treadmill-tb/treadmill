# ubuntu-2204-gha-runner image (amd64, qemu/uefi) — 2 root layers.
#
# Port of the legacy `images/vm-ubuntu-2204-amd64-uefi/gh-actions-overlay.nix`.
# A differential qcow2 overlay over the shared ubuntu-2204 rootfs that installs
# the GitHub Actions self-hosted runner + its `gh-actions-runner.service` unit.
#
# OCI-migration changes vs. the original:
#   * the lower layer is the *shared* `disk-image.qcow2` derivation (the same
#     one the base image ships), so the overlay's `ci.treadmill.qcow2.lower`
#     digest is byte-identical to the base image's head blob — no manifest
#     digging via `dasel` is needed;
#   * the overlay's baked backing path is stripped (`qemu-img rebase -u -b ""`)
#     and the two layers are handed to `mkTreadmillImage`, which rewires the
#     chain (`lower` = base digest, `head` = overlay digest) — replacing the
#     bespoke TOML manifest surgery of the original;
#   * the runner tarball is a `pkgs.fetchurl` FOD (offline-eval-safe), not
#     `builtins.fetchurl`.
{
  pkgs,
  lib,
  mediaTypes,
  mkTreadmillImage,
  # The shared deb-bootstrapped rootfs derivation (import of
  # ../ubuntu-2204/rootfs.nix); `${baseRootfs}/disk-image.qcow2` is the lower
  # (base) qcow2 layer.
  baseRootfs,
}:
let
  ghActionsRunnerVersion = "2.319.1";

  ghActionsRunnerHashes = {
    "x64" = "sha256-P277dIihg+KR/Cxih24Uye5zKGQXNzT6zIWhv7F0RGQ=";
  };

  ghActionsRunnerArch = "x64";

  # `pkgs.fetchurl` (FOD), not `builtins.fetchurl`: see ../ubuntu-2204/rootfs.nix.
  ghActionsRunnerArchive = pkgs.fetchurl {
    url = "https://github.com/actions/runner/releases/download/v${ghActionsRunnerVersion}/actions-runner-linux-${ghActionsRunnerArch}-${ghActionsRunnerVersion}.tar.gz";
    sha256 = ghActionsRunnerHashes."${ghActionsRunnerArch}";
  };

  ghActionsRunnerUnpacked =
    pkgs.runCommand "gh-actions-runner-linux-${ghActionsRunnerArch}-${ghActionsRunnerVersion}-unpack"
      { }
      ''
        mkdir -p $out
        ${pkgs.gnutar}/bin/tar -xzf ${ghActionsRunnerArchive} -C $out
      '';

  # Build the differential overlay inside a Linux VM: create a qcow2 backed by
  # the shared base qcow2, mount its root partition, install the runner + unit,
  # and emit the (still backing-referencing) overlay.qcow2.
  overlayRaw = pkgs.vmTools.runInLinuxVM (
    pkgs.runCommand "ubuntu-2204-gha-runner-overlay-vm"
      {
        memSize = 768;
        preVM = ''
          mkdir -p $out

          # The below variable is magically picked up and exposed to the VM.
          diskImage=disk.qcow2
          echo "Creating $diskImage based on ${baseRootfs}/disk-image.qcow2"
          ${pkgs.qemu}/bin/qemu-img create \
            -b "${baseRootfs}/disk-image.qcow2" -F qcow2 -f qcow2 "$diskImage"
        '';
        postVM = ''
          mkdir -p $out
          cp $diskImage $out/overlay.qcow2
        '';
        buildInputs = [ ];
      }
      ''
        mkdir -p /mnt
        ${pkgs.mount}/bin/mount /dev/vda2 /mnt
        ls /mnt

        cp -r ${ghActionsRunnerUnpacked} /mnt/opt/gh-actions-runner
        chown 1000:1000 -R /mnt/opt/gh-actions-runner
        chmod u+w -R /mnt/opt/gh-actions-runner

        cat > "/mnt/etc/systemd/system/gh-actions-runner.service" <<EOF
        [Unit]
        Description=GitHub Actions Runner
        After=network.target tml-puppet.service
        Wants=tml-puppet.service

        [Service]
        ExecStartPre=/bin/bash -Eexuo pipefail -c '${
          lib.concatStringsSep " && " [
            # Services are supposed to use the `runsvc.sh` script, which will be
            # created by another setup script in the repository. We simply copy it
            # manually here.
            #
            # This script is not actually used right now; see below.
            "cp /opt/gh-actions-runner/bin/runsvc.sh /opt/gh-actions-runner/runsvc.sh"
            "chown tml:tml /opt/gh-actions-runner/runsvc.sh"
            # Avoid re-configuring if the runner was already configured:
            "if [ -f /opt/gh-actions-runner/.credentials ]; then exit 0; fi"
            # Avoid configuring if we have a JIT configuration:
            "if [ -f /run/tml/parameters/gh-actions-runner-encoded-jit-config ]; then exit 0; fi"
            # Read the configuration parameters and run the configuration script:
            "REPO_URL=\\\$(cat /run/tml/parameters/gh-actions-runner-repo-url)"
            "RUNNER_TOKEN=\\\$(cat /run/tml/parameters/gh-actions-runner-token)"
            "JOB_ID=\\\$(cat /run/tml/job-id)"
            (lib.concatStringsSep " " [
              "/opt/gh-actions-runner/config.sh"
              "--url \\\$REPO_URL"
              "--token \\\$RUNNER_TOKEN"
              "--name tml-gh-actions-runner-\\\$JOB_ID"
              "--labels tml-gh-actions-runner-\\\$JOB_ID"
              "--unattended"
              "--ephemeral"
            ])
          ]
        }'
        ExecStartPre=-+/bin/bash /run/tml/parameters/gh-actions-runner-exec-start-pre-sh
        ExecStart=/bin/bash -Eeuo pipefail -c '\
          if [ -f /run/tml/parameters/gh-actions-runner-encoded-jit-config ]; then \
            ${
              ""
              # We should use runsvc.sh here, but it doesn't support the jitconfig
              # option. For now, run.sh seems to work fine as well.
            } \
            echo "Starting GitHub Actions Runner from JIT config"; \
            /opt/gh-actions-runner/run.sh --jitconfig \$(cat /run/tml/parameters/gh-actions-runner-encoded-jit-config); \
          else \
            ${
              ""
              # To have a compatible "KillSignal" to the above, also use run.sh:
            } \
            echo "Starting preconfigured GitHub Actions Runner"; \
            /opt/gh-actions-runner/run.sh; \
          fi'
        Restart=on-failure
        # KillMode=process, for runsvc.sh
        # KillSignal=SIGTERM, for runsvc.sh
        KillSignal=SIGINT # for run.sh
        TimeoutStopSec=5m
        User=tml
        Group=tml
        WorkingDirectory=/opt/gh-actions-runner
        ExecStopPost=-+/bin/bash /run/tml/parameters/gh-actions-runner-exec-stop-post-sh

        [Install]
        WantedBy=multi-user.target
        EOF

        # Manually enable the service:
        ln -s /etc/systemd/system/gh-actions-runner.service /mnt/etc/systemd/system/multi-user.target.wants/gh-actions-runner.service

        cat > /mnt/opt/journal-login-shell <<SCRIPT
        #!/bin/bash
        exec /bin/bash --init-file <(echo 'sudo journalctl -f; . "\$HOME/.bashrc"')
        SCRIPT
        chmod +x /mnt/opt/journal-login-shell
        sed -i -E 's|^(tml:.*):/bin/bash$|\1:/opt/journal-login-shell|' /mnt/etc/passwd
      ''
  );

  # Strip the baked backing path so `mkTreadmillImage` can rewire the chain
  # (`lower` = base digest) — mirrors the legacy `qemu-img rebase -u -b ""`.
  overlayBlob =
    pkgs.runCommand "ubuntu-2204-gha-runner-overlay" { nativeBuildInputs = [ pkgs.qemu-utils ]; }
      ''
        mkdir -p $out
        cp ${overlayRaw}/overlay.qcow2 $out/overlay.qcow2
        chmod +w $out/overlay.qcow2
        qemu-img rebase -u -b "" -f qcow2 $out/overlay.qcow2
      '';
in
mkTreadmillImage {
  name = "ubuntu-2204-gha-runner";
  title = "Ubuntu 22.04 with GitHub Actions Runner";
  description = "Base Ubuntu 22.04 with added GitHub Actions Runner service and scripts.";
  layers = [
    {
      # Lower (base) layer — identical blob to the ubuntu-2204 head.
      path = "${baseRootfs}/disk-image.qcow2";
      mediaType = mediaTypes.diskQcow2;
      role = "root";
      virtualSize = null;
    }
    {
      # Head (overlay) layer — the differential with its backing stripped.
      path = "${overlayBlob}/overlay.qcow2";
      mediaType = mediaTypes.diskQcow2;
      role = "root";
      virtualSize = null;
    }
  ];
}
