# raspbian-13 shared build parts: the customized SD image and the boot FAT /
# root qcow2 blobs extracted from it.
#
# Factored out of ./default.nix (the "customize RPi OS under TCG qemu" half of
# the legacy `images/netboot-raspberrypi-nbd/default.nix`) so that BOTH the base
# image and the gha-runner overlay consume the exact same blobs — the overlay's
# `ci.treadmill.qcow2.lower` digest must be byte-identical to the base image's
# head (root) blob, and the boot FAT layer is shared verbatim. Sharing one set
# of derivations guarantees that.
#
# OCI-migration changes vs. the original are documented on ./default.nix.
{
  pkgs,
  lib,
  # The statically-linked aarch64 musl `tml-puppet` package
  # (self'.packages.tml-puppet-static-aarch64).
  puppet,
}:
let
  inherit (pkgs)
    bash
    coreutils
    qemu
    libraspberrypi
    xz
    p7zip
    lkl
    e2fsprogs
    fetchurl
    writeText
    writeScript
    runCommand
    ;

  nbdClientDeb = fetchurl {
    url = "https://alpha.mirror.svc.schuermann.io/files/treadmill-tb/nbd-client_3.24-1.1_arm64.deb";
    sha256 = "sha256-SM5aIKwFqggjqzlJlZQr6tj1UfkA5f+VmrW//m/yJtk=";
  };

  # `pkgs.fetchurl` (FOD), not `builtins.fetchurl`: the latter downloads during
  # evaluation, breaking offline eval. The FOD output path is byte-identical to
  # the legacy `builtins.fetchurl` store path (flat sha256 + basename).
  rustupInit = fetchurl {
    url = "https://alpha.mirror.svc.schuermann.io/files/treadmill-tb/2024-12-06_rustup-init_aarch64-unknown-linux-gnu";
    sha256 = "1cm2vdwf4r7gxk4n7qqxpsfvvg5v943fa7bldxs4qqrywr8vzzqw";
  };

  raspberryPiOSImage = fetchurl {
    urls = [
      "https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-04-21/2026-04-21-raspios-trixie-arm64-lite.img.xz"
      "https://alpha.mirror.svc.schuermann.io/files/treadmill-tb/2026-04-21-raspios-trixie-arm64-lite.img.xz"
    ];
    sha256 = "sha256-TNMd8Cb9giQ4BaMm3Ayv1zg/fj0wyUE+cETVB6rigeI=";
  };

  autologinDevices = [
    "ttyAMA0"
    "ttyAMA10"
  ];

  autologinOverride =
    targetUser:
    writeText "autologin-override.conf" ''
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin ${targetUser} --noclear %I $TERM
    '';

  nbdFstab = writeText "fstab" ''
    proc /proc proc defaults 0 0
    # PARTUUID=d28ec40f-01 /boot/firmware vfat defaults 0 2
    /dev/nbd0 / ext4 defaults,noatime,nodiratime 0 1
  '';

  customizeImageScript = writeScript "customize-image.sh" ''
    #!/bin/sh

    set -o xtrace
    set -e

    # Normally done by init system:
    mkdir -p /proc /sys
    mount -t proc proc /proc
    mount -t sysfs sysfs /sys

    # Required mountpoint for update-initramfs and cmdline.txt update
    mount /dev/mmcblk0p1 /boot/firmware

    # Update cmdline.txt for nbd boot:
    echo "console=serial0,115200 ip=dhcp root=/dev/nbd0 rw nbdroot=dhcp,root,nbd0 rootfstype=ext4 fsckfix rootwait net.ifnames=0 loglevel=7" > /boot/firmware/cmdline.txt

    # Override fstab to mount the nbd volume as root:
    mv /customize-image/fstab /etc/fstab

    # Disable services that fail without /boot/firmware or without an SD card:
    ln -s /dev/null /etc/systemd/system/dphys-swapfile.service
    ln -s /dev/null /etc/systemd/system/rpi-eeprom-update.service
    ln -s /dev/null /etc/systemd/system/userconfig.service
    ln -s /dev/null /etc/systemd/system/systemd-hostnamed.service
    ln -s /dev/null /etc/systemd/system/systemd-hostnamed.socket
    ln -s /dev/null /etc/systemd/system/systemd-logind.service

    # Mask cloud-init (not needed for this custom provisioning)
    ln -s /dev/null /etc/systemd/system/cloud-init.service
    ln -s /dev/null /etc/systemd/system/cloud-init-local.service
    ln -s /dev/null /etc/systemd/system/cloud-init-network.service
    ln -s /dev/null /etc/systemd/system/cloud-final.service
    ln -s /dev/null /etc/systemd/system/cloud-init.target

    # Mask SD-card specific resize and swap services incompatible with NBD
    ln -s /dev/null /etc/systemd/system/rpi-resize.service
    ln -s /dev/null /etc/systemd/system/systemd-growfs-root.service
    ln -s /dev/null /etc/systemd/system/rpi-resize-swap-file.service

    # Mask sshswitch (fails because /boot/firmware is a mock directory)
    ln -s /dev/null /etc/systemd/system/sshswitch.service

    # Create a mock firmware directory, in lieu of mounting the actual TFTP
    # boot file system:
    mkdir -p /boot/firmware-mock
    echo "FWLOC=/boot/firmware-mock" > /etc/default/raspberrypi-sys-mods

    # Enable ssh on first boot (picked up by sshswitch.service)
    #
    # Write to both the actual firmware file system and the mock FS, to
    # enable SSHD regardless of which will get mounted.
    touch /boot/firmware/ssh.txt
    touch /boot/firmware-mock/ssh.txt

    # Expand root partition on first boot
    mv /customize-image/expandroot.sh /opt/expandroot
    chmod +x /opt/expandroot
    touch /firstboot-expandroot
    cat > /etc/systemd/system/firstboot-expandroot.service <<SERVICE
    [Install]
    WantedBy=multi-user.target
    [Unit]
    ConditionPathExists=/firstboot-expandroot
    [Service]
    Type=simple
    ExecStart=/opt/expandroot
    ExecStartPost=/bin/rm /firstboot-expandroot
    SERVICE
    ln -s /etc/systemd/system/firstboot-expandroot.service /etc/systemd/system/multi-user.target.wants/firstboot-expandroot.service

    # Delete the pre-created pi user:
    userdel -r pi

    # Create a treadmill user in the image and give it password-less sudo:
    useradd -m -u 1000 -s /bin/bash tml
    usermod -a -G plugdev tml
    usermod -a -G tty tml
    usermod -a -G dialout tml
    usermod -a -G gpio tml
    cat <<EOF >/etc/sudoers.d/010_tml-nopasswd
    tml ALL=(ALL) NOPASSWD: ALL
    EOF

    # Give access to all USB devices
    cat > /etc/udev/rules.d/99-tml.rules <<RULES
    SUBSYSTEM=="usb", GROUP="plugdev", TAG+="uaccess"
    RULES

    # Auto-login to the tml user on select serial consoles:
    ${lib.concatStringsSep "\n" (
      builtins.map (device: ''
        mkdir -p /etc/systemd/system/serial-getty@${device}.service.d/
        cp /customize-image/autologin-override.conf /etc/systemd/system/serial-getty@${device}.service.d/override.conf
      '') autologinDevices
    )}

    # Move the puppet daemon to /opt, autostart and always restart on exit:
    mkdir -p /opt
    mv /customize-image/tml-puppet /usr/local/bin/tml-puppet
    # Type=notify: Only report as started after connected to supervisor
    # NotifyAccess=main: Don't accept status updates from child processes
    cat > /etc/systemd/system/tml-puppet.service <<SERVICE
    [Install]
    WantedBy=multi-user.target
    [Unit]
    After=network.target
    StartLimitIntervalSec=0
    [Service]
    Type=notify
    NotifyAccess=main
    ExecStartPre=/bin/mkdir -p /run/tml/parameters /home/tml/.ssh
    ExecStartPre=/usr/bin/touch /home/tml/.ssh/authorized_keys
    ExecStartPre=/bin/chmod 500 /home/tml/.ssh
    ExecStartPre=/bin/chown -R tml /home/tml/.ssh
    ExecStart=/bin/bash -c '/usr/local/bin/tml-puppet daemon --transport tcp --tcp-control-socket-addr "\$(ip route show 0.0.0.0/0 | cut -d" " -f3 | head -n1):3859" --authorized-keys-file /home/tml/.ssh/authorized_keys --exit-on-authorized-keys-update-error --job-info-dir /run/tml --parameters-dir /run/tml/parameters'
    Restart=always
    RestartSec=5s
    SERVICE
    ln -s /etc/systemd/system/tml-puppet.service /etc/systemd/system/multi-user.target.wants/tml-puppet.service

    # Allow the puppet daemon to bind to its D-Bus service
    cat > /etc/dbus-1/system.d/ci.treadmill.Puppet.conf <<DBUSCONF
    <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
    <busconfig>
      <policy context="default">
        <allow own="ci.treadmill.Puppet"/>
        <allow send_destination="ci.treadmill.Puppet"/>
        <allow receive_sender="ci.treadmill.Puppet"/>
      </policy>
    </busconfig>
    DBUSCONF

    # Install rustup as the tml user:
    chmod +x /customize-image/rustup-init
    sudo -u tml /customize-image/rustup-init -y --default-toolchain none --profile minimal

    # Install nbd-client
    apt install "/customize-image/nbd-client_3.24-1.1_arm64.deb"

    # Delete the customize-image files:
    rm -rf /customize-image

    # Unmount all disk (read-only remount root fs) and force power off.
    # We don't have an init system active:
    umount /boot/firmware

    # This will not work, "mount point is busy":
    #mount -o remount,ro /dev/mmcblk0p2 /

    echo s > /proc/sysrq-trigger
    echo u > /proc/sysrq-trigger

    # Give the system a little time to flush changes to disk
    sleep 5

    poweroff -f
  '';

  # Boot the stock RPi OS image under TCG qemu-system-aarch64 with our custom
  # init, producing the customized raw SD image. The dtb / kernel8 outputs are
  # retained for a future netboot run harness (§6.6); the image builds only
  # consume `out`.
  customizedSDImage = derivation {
    name = "treadmill-image-raspbian-13-customized-sd";
    system = pkgs.stdenv.hostPlatform.system;
    builder = "${bash}/bin/bash";
    outputs = [
      "out"
      "qemuBcm2710Rpi3BPlusDtb"
      "kernel8"
    ];
    args = [
      "-c"
      ''
        set -euo pipefail

        ${xz}/bin/xz -d --stdout ${raspberryPiOSImage} > raspios.img
        ${p7zip}/bin/7z -y x raspios.img 0.fat
        ${coreutils}/bin/mv 0.fat boot.img
        ${p7zip}/bin/7z -y x boot.img bcm2710-rpi-3-b-plus.dtb bcm2711-rpi-4-b.dtb kernel8.img overlays/disable-bt.dtbo
        ${coreutils}/bin/test -f bcm2710-rpi-3-b-plus.dtb
        ${coreutils}/bin/test -f bcm2711-rpi-4-b.dtb
        ${coreutils}/bin/test -f kernel8.img
        ${coreutils}/bin/test -f overlays/disable-bt.dtbo

        # We need to patch the DTB to enable UART console output for the
        # kernel and disable Bluetooth -- that'll hang QEMU. This doesn't
        # touch the DTB in the image itself:
        ${coreutils}/bin/cp bcm2710-rpi-3-b-plus.dtb bcm2710-rpi-3-b-plus.dtb.cust
        ${libraspberrypi}/bin/dtmerge bcm2710-rpi-3-b-plus.dtb.cust bcm2710-rpi-3-b-plus.dtb.merged - uart0=on
        ${coreutils}/bin/mv bcm2710-rpi-3-b-plus.dtb.merged bcm2710-rpi-3-b-plus.dtb.cust
        ${libraspberrypi}/bin/dtmerge bcm2710-rpi-3-b-plus.dtb.cust bcm2710-rpi-3-b-plus.dtb.merged overlays/disable-bt.dtbo
        ${coreutils}/bin/mv bcm2710-rpi-3-b-plus.dtb.merged bcm2710-rpi-3-b-plus.dtb.cust

        # Copy the nbd-client and image customization script in:
        ${coreutils}/bin/mkdir ./customize-image
        ${coreutils}/bin/cp -L ${customizeImageScript} ./customize-image/customize.sh
        ${coreutils}/bin/cp -L ${nbdFstab} ./customize-image/fstab
        ${coreutils}/bin/cp -L ${autologinOverride "tml"} ./customize-image/autologin-override.conf
        ${coreutils}/bin/cp -L ${../lib/expandroot.sh} ./customize-image/expandroot.sh
        ${coreutils}/bin/cp -L ${nbdClientDeb} ./customize-image/nbd-client_3.24-1.1_arm64.deb
        ${coreutils}/bin/cp -L ${puppet}/bin/tml-puppet ./customize-image/tml-puppet
        ${coreutils}/bin/cp -L ${rustupInit} ./customize-image/rustup-init
        ${lkl.out}/bin/cptofs -p -t ext4 -P2 -i raspios.img customize-image /

        # QEMU requires images to be a power of two in size:
        ${qemu}/bin/qemu-img resize -f raw ./raspios.img 4G

        # Perform the remaining image customizations in a VM:
        ${qemu}/bin/qemu-system-aarch64 \
          -machine raspi3b -cpu cortex-a72 -m 1G -smp 4 \
          -dtb bcm2710-rpi-3-b-plus.dtb.cust \
          -kernel kernel8.img \
          -device sd-card,drive=drive0 -drive id=drive0,if=none,format=raw,file=raspios.img \
          -append "rw earlyprintk loglevel=8 console=ttyAMA0,115200 dwc_otg.lpm_enable=0 root=/dev/mmcblk0p2 rootdelay=1 init=/customize-image/customize.sh" \
          -nographic

        ${coreutils}/bin/mv raspios.img $out
        ${coreutils}/bin/mv bcm2710-rpi-3-b-plus.dtb.cust $qemuBcm2710Rpi3BPlusDtb
        ${coreutils}/bin/mv kernel8.img $kernel8
      ''
    ];
  };

  # Boot blob: the raw FAT boot partition, stored DIRECTLY (no tar — plan I10).
  bootFat = runCommand "treadmill-image-raspbian-13-bootfat" { } ''
    ${p7zip}/bin/7z x ${customizedSDImage} 0.fat
    ${coreutils}/bin/mv 0.fat $out
  '';

  # Root blob: the ext4 root partition, fscked and converted to qcow2.
  rootQcow2 = runCommand "treadmill-image-raspbian-13-root-qcow2" { } ''
    ${p7zip}/bin/7z x ${customizedSDImage} 1.img
    ${e2fsprogs}/bin/e2fsck -y -f 1.img || [ $? -eq 1 ]
    ${qemu}/bin/qemu-img convert -f raw -O qcow2 1.img $out
  '';
in
{
  inherit
    customizedSDImage
    bootFat
    rootQcow2
    ;
}
