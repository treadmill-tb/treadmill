# ubuntu-2204 deb-bootstrapped root filesystem (amd64, qemu/uefi).
#
# This is the `vmTools.makeImageFromDebDist` half of the legacy
# `images/vm-ubuntu-2204-amd64-uefi/default.nix`, factored out of the OCI
# wrapper (./default.nix) so that BOTH the base image and the gha-runner
# overlay can consume the exact same `disk-image.qcow2`. Sharing one derivation
# guarantees the overlay's `ci.treadmill.qcow2.lower` digest is byte-identical
# to the base image's head blob (a differential overlay backed by a different
# base is meaningless).
#
# Faithful to the original modulo the two OCI-migration changes documented on
# ./default.nix: the puppet binary comes from the in-tree static package
# (`puppet` arg), and rustup-init is a `pkgs.fetchurl` FOD.
{
  pkgs,
  lib,
  # The statically-linked x86_64 musl `tml-puppet` package
  # (self'.packages.tml-puppet-static-x86_64).
  puppet,
}:
let
  inherit (pkgs)
    vmTools
    gptfdisk
    util-linux
    dosfstools
    e2fsprogs
    systemd
    coreutils
    fetchurl
    ;

  distro = vmTools.debDistros.ubuntu2204x86_64;
  distroRepoName = "jammy";
  disk = "/dev/vda";

  # `pkgs.fetchurl` (a fixed-output derivation), not the legacy
  # `builtins.fetchurl`: the latter downloads during *evaluation*, so the flake
  # can't even be evaluated offline. As an FOD this is fetched lazily at build
  # time and is substitutable/cacheable.
  rustupInit = fetchurl {
    url = "https://alpha.mirror.svc.schuermann.io/files/treadmill-tb/2025-04-28_rustup-init_x86_64-unknown-linux-gnu";
    sha256 = "0zryrpk9xwxk3rcic3f1x6ymrzk1300cw2xscacbpl630jq9ycx3";
  };
in
vmTools.makeImageFromDebDist rec {
  inherit (distro)
    name
    fullName
    urlPrefix
    packagesLists
    ;

  # We don't want to expose a variable called "diskImage" which is defined by
  # "createEmptyImage". This variable will be automatically exposed as a
  # virtio-blk device to the QEMU VM and its options cannot be customized
  # (without interpolating them into the device name, which we will _not_ do
  # to retain a tiny bit of sanity in this whole mess).
  #
  # We instead use the variable "customDiskImage" to hold this path, and
  # expose it to the VM with "discard=unmap" and "detect-zeroes=unmap", such
  # that the fstrim command in the VM can actually free blocks allocated in
  # the underlying QCOW2 file after installation.
  preVM =
    (vmTools.createEmptyImage {
      inherit size fullName;
    })
    + ''
      customDiskImage="$diskImage"
      unset diskImage
      echo "Custom disk image: $customDiskImage"
    '';

  # For some reason, a SCSI device will be picked up by the kernel but has no
  # node created in the devtmpfs at /dev:
  #
  # QEMU_OPTS = "-device virtio-scsi-pci,id=scsi0 \
  #   -drive file=$customDiskImage,id=drive0,format=qcow2,cache=unsafe,werror=report,discard=unmap,if=none \
  #   -device scsi-hd,drive=drive0,bus=scsi0.0";
  #
  # Instead, it seems that with recent versions of QEMU, discard=unmap and
  # detect-zeroes=unmap also achieves the desired result despite using
  # virtio-blk:
  QEMU_OPTS = "-drive file=$customDiskImage,if=virtio,cache=unsafe,werror=report,discard=unmap,detect-zeroes=unmap";

  packages =
    (lib.filter (
      p:
      !lib.elem p [
        "g++"
        "make"
        "dpkg-dev"
        "pkg-config"
        "sysvinit"
      ]
    ) distro.packages)
    ++ [
      "systemd"
      "init-system-helpers"
      "systemd-sysv"
      "linux-image-generic"
      "initramfs-tools"
      "e2fsprogs"
      "grub-efi"
      "apt"
      "openssh-server"
      "sudo"
      "iproute2"
      "terminfo"
      "ncurses-bin"
      "ncurses-term"
      "cloud-guest-utils" # growpart
      "git"
      "build-essential"
      "usbutils"
      "pciutils"
      "vim"
      "tmux"
      "htop"
      "nload"
      "nano"
      "gnupg"
      "bc"
      "mtr"
      "zip"
      "unzip"
      "wget"
      "ping"
      "ca-certificates"
      "dbus"
    ];

  size = 5 * 1024; # Minimum image size, 5GB

  createRootFS = ''
    ${gptfdisk}/bin/sgdisk "${disk}" \
      -n1:0:+100M -t1:ef00 -c1:esp \
      -n2:0:0 -t2:8300 -c2:root

    ${util-linux}/bin/partx -u "${disk}"
    ${dosfstools}/bin/mkfs.vfat -F32 -n ESP "${disk}1"
    ${e2fsprogs}/bin/mkfs.ext4 "${disk}2" -L root
    # Required for grub-install --target=x86_64-efi to work
    ${e2fsprogs}/bin/tune2fs -O ^metadata_csum_seed "${disk}2"
    # Required for e2fsck to work in the final system image
    ${e2fsprogs}/bin/tune2fs -O ^orphan_file "${disk}2"

    mkdir /mnt
    ${util-linux}/bin/mount -t ext4 "${disk}2" /mnt
    mkdir -p /mnt/{proc,dev,sys,boot/efi}
    ${util-linux}/bin/mount -t vfat "${disk}1" /mnt/boot/efi

    touch /mnt/.debug
  '';

  postInstall = ''
    # update-grub needs udev to detect the filesystem UUID -- without,
    # we'll get root=/dev/vda2 on the cmdline which will only work in
    # a limited set of scenarios.
    ${systemd}/lib/systemd/systemd-udevd &
    ${systemd}/bin/udevadm trigger
    ${systemd}/bin/udevadm settle

    ${util-linux}/bin/mount -t proc proc /mnt/proc
    ${util-linux}/bin/mount -t sysfs sysfs /mnt/sys
    ${util-linux}/bin/mount -o bind /dev /mnt/dev
    ${util-linux}/bin/mount -o bind /dev/pts /mnt/dev/pts

    # Copy the treadmill puppet binary into the root file system:
    mkdir -p /mnt/usr/local/bin/
    cp ${puppet}/bin/tml-puppet /mnt/usr/local/bin/tml-puppet

    # Copy rustup-init binary and the rustup-init config into the image
    mkdir -p /mnt/opt/
    cp ${rustupInit} /mnt/opt/rustup-init
    chmod +x /mnt/opt/rustup-init

    # Copy the expandroot script:
    cp ${../lib/expandroot.sh} /mnt/opt/expandroot
    chmod +x /mnt/opt/expandroot

    chroot /mnt /bin/bash -exuo pipefail <<CHROOT
    export PATH=/usr/sbin:/usr/bin:/sbin:/bin
    find /usr/sbin/
    find /usr/bin/
    find /sbin/
    find /bin/

    # update-initramfs needs to know where its root filesystem lives,
    # so that the initial userspace is capable of finding and mounting it.
    echo "/dev/disk/by-uuid/$(${util-linux}/bin/blkid -s UUID -o value "${disk}2") / ext4 defaults" > /etc/fstab
    echo "/dev/disk/by-uuid/$(${util-linux}/bin/blkid -s UUID -o value "${disk}1") /boot/efi vfat defaults" >> /etc/fstab

    cat /etc/fstab

    # rebuild the initramfs
    update-initramfs -k all -c

    # APT sources so we can update the system and install new packages
    cat > /etc/apt/sources.list <<SOURCES
    deb http://archive.ubuntu.com/ubuntu ${distroRepoName} main restricted universe
    deb http://security.ubuntu.com/ubuntu ${distroRepoName}-security main restricted universe
    deb http://archive.ubuntu.com/ubuntu ${distroRepoName}-updates main restricted universe
    SOURCES

    # Install the boot loader to the EFI System Partition
    # Remove "quiet" from the command line so that we can see what's happening during boot,
    # and enable the grub terminal on the serial console (no monitor attached)
    cat >> /etc/default/grub <<EOF
    GRUB_TIMEOUT=5
    GRUB_CMDLINE_LINUX="console=ttyS0"
    GRUB_CMDLINE_LINUX_DEFAULT=""
    GRUB_TERMINAL="serial"
    EOF
    sed -i '/TIMEOUT_HIDDEN/d' /etc/default/grub
    update-grub
    grub-install --target=x86_64-efi --efi-directory=/boot/efi --removable

    # Configure networking
    ln -snf /lib/systemd/resolv.conf /etc/resolv.conf
    systemctl enable systemd-networkd systemd-resolved
    cat >/etc/systemd/network/10-eth.network <<NETWORK
    [Match]
    Name=en*
    Name=eth*
    [Link]
    RequiredForOnline=true
    [Network]
    DHCP=yes
    NETWORK

    # Expand root partition on first boot
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

    # Configure SSH
    #
    # Type=oneshot will ensure that ssh.service waits on this to complete
    #
    # WantedBy=ssh.service would prevent SSH from starting if this isn't
    # executed, so we use WantedBy=multi-user.target (same as ssh.service)
    rm /etc/ssh/ssh_host_*_key
    cat > /etc/systemd/system/ssh-generate-host-keys.service <<SERVICE
    [Install]
    WantedBy=multi-user.target
    [Unit]
    Before=ssh.service
    ConditionPathExistsGlob=!/etc/ssh/ssh_host_*_key
    [Service]
    Type=oneshot
    ExecStart=/usr/sbin/dpkg-reconfigure openssh-server
    RemainAfterExit=true
    SERVICE
    systemctl enable ssh-generate-host-keys

    # The above calls \`dpkg-reconfigure openssh-server\` on the first system
    # boot, and delays starting the SSH server until this command completes.
    #
    # However, \`dpkg-reconfigure openssh-server\` will also try to _start_ the
    # SSH server by running \`systemctl start ssh.service\`. This command will
    # in turn block until the above \`ssh-generate-host-keys.service\` is
    # finished ... which blocks on SSH starting!
    #
    # Thanks to the _amazingly great_ design decision to start services on
    # installation we thus have to use a workaround here. Luckily, Debian
    # carries ages of legacy with it. And so, the postinstall scripts of
    # ssh.service don't directly run \`systemctl\`. No, instead they run a tool
    # called \`deb-systemd-invoke\` from a time long past that will consult
    # policy files written for the rc.d init system. And those policy files
    # are seemingly still the only way to prevent Debian packages from
    # automatically starting on installation.
    #
    # Hence, with this policy file in place, \`dpkg-reconfigure openssh-server\`
    # can run to completion, and then systemd will happily start ssh.service
    # afterwards. Easy! If only everything in life was that simple.
    cat > /usr/sbin/policy-rc.d <<POLICY
    #!/bin/bash
    SERVICE_NAME="\\\$(ps -o command= --ppid \\\$PPID | cut -d ' ' -f 3 )"
    echo "Filtering SSH services for dpkg maintscript-triggered restarts: \\\$SERVICE_NAME" >&2
    if [[ "\\\$SERVICE_NAME" == "ssh" ]] \
        || [[ "\\\$SERVICE_NAME" == "ssh.service" ]] \
        || [[ "\\\$SERVICE_NAME" == "rescue-ssh.service" ]]; then
      exit 101
    fi
    exit 0
    POLICY
    chmod a+rx /usr/sbin/policy-rc.d

    # As the above causes SSH to no longer be enabled on install, let's
    # enable it here for good measure (it should already be enabled by the
    # previous implicit invocation of \`dpkg-reconfigure\`):
    systemctl enable ssh.service

    # Create treadmill user and enable password-less sudo and autologin
    useradd -m -u 1000 -s /bin/bash tml
    usermod -a -G plugdev tml
    usermod -a -G tty tml
    echo "tml ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers.d/010_tml-nopasswd
    mkdir -p /etc/systemd/system/serial-getty@ttyS0.service.d/
    cat > /etc/systemd/system/serial-getty@ttyS0.service.d/override.conf <<SERVICE
    [Service]
    ExecStart=
    ExecStart=-/sbin/agetty --autologin tml --noclear %I $TERM
    SERVICE

    # Give access to all USB devices
    cat > /etc/udev/rules.d/99-tml.rules <<RULES
    SUBSYSTEM=="usb", GROUP="plugdev", TAG+="uaccess"
    RULES

    # Autostart the treadmill puppet daemon and always restart on exit:
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
    ExecStart=/usr/local/bin/tml-puppet daemon --transport auto_discover --authorized-keys-file /home/tml/.ssh/authorized_keys --exit-on-authorized-keys-update-error --job-info-dir /run/tml --parameters-dir /run/tml/parameters
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

    # Install rustup-init as the tml user:
    sudo -u tml /opt/rustup-init -y --default-toolchain none --profile minimal

    CHROOT

    echo "Disk utilization after installation:"
    ${coreutils}/bin/df -h

    echo "Running fstrim to unmap unused but allocated blocks in the underlying image file..."
    ${util-linux}/bin/fstrim -a -v

    ${util-linux}/bin/umount /mnt/dev/pts
    ${util-linux}/bin/umount /mnt/dev
    ${util-linux}/bin/umount /mnt/sys
    ${util-linux}/bin/umount /mnt/proc
    ${util-linux}/bin/umount /mnt/boot/efi
  '';

  # Compress the QCOW2 file. This does not work inplace, unfortunately:
  postVM = ''
    diskImage="$customDiskImage"
    echo "Uncompressed image size: $(du -h "$diskImage")"
    echo "Compressing image..."
    ${vmTools.qemu}/bin/qemu-img convert -c "$diskImage" -O qcow2 "$diskImage_compressed.qcow2"
    mv "$diskImage_compressed.qcow2" "$diskImage"
    echo "Compressed image size: $(du -h "$diskImage")"
  '';
}
