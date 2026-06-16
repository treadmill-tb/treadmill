# raspbian-13-gha-runner image (arm64, nbd-netboot) — boot FAT + 2 root layers.
#
# Port of `images/netboot-raspberrypi-nbd/gh-actions-runner-overlay.nix`. A
# differential qcow2 overlay over the shared raspbian-13 root qcow2 that adds an
# `install-gh-actions-runner.service` (downloads the latest runner release at
# first boot) and the `gh-actions-runner.service` unit. The boot FAT layer is
# shared verbatim with the base image.
#
# OCI-migration changes vs. the original:
#   * the lower root layer is the *shared* root qcow2 derivation (the same blob
#     the base image ships), so the overlay's `ci.treadmill.qcow2.lower` digest
#     is byte-identical to the base head blob — no manifest digging via `dasel`;
#   * the overlay's baked backing path is stripped (`qemu-img rebase -u -b ""`)
#     and the three layers are handed to `mkTreadmillImage`, which wires the
#     chain (boot standalone; base root no lower; overlay root lower = base;
#     head = overlay) — replacing the bespoke TOML manifest surgery.
{
  pkgs,
  lib,
  mediaTypes,
  mkTreadmillImage,
  # The shared build parts (import of ./parts.nix): { bootFat; rootQcow2; ... }.
  parts,
}:
let
  # Build the differential overlay inside a Linux VM: create a qcow2 backed by
  # the shared root qcow2 (expanded to 4G), grow the filesystem, install the
  # runner units, and emit the (still backing-referencing) overlay.qcow2.
  overlayRaw = pkgs.vmTools.runInLinuxVM (
    pkgs.runCommand "raspbian-13-gha-runner-overlay-vm"
      {
        memSize = 768;
        preVM = ''
          mkdir -p $out

          # The below variable is magically picked up and exposed to the VM.
          diskImage=disk.qcow2
          echo "Creating and expanding $diskImage based on ${parts.rootQcow2}"
          ${pkgs.qemu}/bin/qemu-img create \
            -b "${parts.rootQcow2}" -F qcow2 -f qcow2 "$diskImage" 4G
        '';
        postVM = ''
          mkdir -p $out
          cp $diskImage $out/overlay.qcow2
        '';
        buildInputs = [ ];
      }
      ''
        mkdir -p /mnt
        ${pkgs.e2fsprogs}/bin/e2fsck -yf /dev/vda
        ${pkgs.e2fsprogs}/bin/resize2fs /dev/vda

        ${pkgs.mount}/bin/mount /dev/vda /mnt
        ls /mnt

        cp ${./install-gh-actions.sh} /mnt/opt/install-gh-actions-runner.sh
        chmod +x /mnt/opt/install-gh-actions-runner.sh
        cat > "/mnt/etc/systemd/system/install-gh-actions-runner.service" <<EOF
        [Unit]
        Description=Download and install latest GitHub Actions Runner release
        After=network.target

        [Service]
        Type=oneshot
        ExecStart=/opt/install-gh-actions-runner.sh
        Restart=on-failure

        User=root
        Group=root
        Environment="RUNNER_ARCH=arm64"
        Environment="RUNNER_OWNER=1000"
        Environment="RUNNER_GROUP=1000"

        [Install]
        WantedBy=multi-user.target
        EOF

        cat > "/mnt/etc/systemd/system/gh-actions-runner.service" <<EOF
        [Unit]
        Description=GitHub Actions Runner
        After=network.target tml-puppet.service install-gh-actions-runner.service
        Wants=tml-puppet.service install-gh-actions-runner.service

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
  # (`lower` = base root digest) — mirrors the legacy `qemu-img rebase -u -b ""`.
  overlayBlob =
    pkgs.runCommand "raspbian-13-gha-runner-overlay" { nativeBuildInputs = [ pkgs.qemu-utils ]; }
      ''
        mkdir -p $out
        cp ${overlayRaw}/overlay.qcow2 $out/overlay.qcow2
        chmod +w $out/overlay.qcow2
        qemu-img rebase -u -b "" -f qcow2 $out/overlay.qcow2
      '';
in
# Boot FAT layer standalone; base root then overlay root form the chain
# (head = overlay).
mkTreadmillImage {
  name = "raspbian-13-gha-runner";
  title = "Raspberry Pi OS 13 (NBD) with GitHub Actions Runner";
  layers = [
    {
      path = "${parts.bootFat}";
      mediaType = mediaTypes.bootFatV1;
      role = "boot";
    }
    {
      # Lower (base) root layer — identical blob to the raspbian-13 head.
      path = "${parts.rootQcow2}";
      mediaType = mediaTypes.diskQcow2;
      role = "root";
      virtualSize = null;
    }
    {
      # Head (overlay) root layer — the differential with its backing stripped.
      path = "${overlayBlob}/overlay.qcow2";
      mediaType = mediaTypes.diskQcow2;
      role = "root";
      virtualSize = null;
    }
  ];
}
