{
  lib,
  vmTools,
  udev,
  gptfdisk,
  util-linux,
  dosfstools,
  e2fsprogs,
  systemd,
  coreutils,
  tree,
  bash,
}: let
  ubuntuImage = vmTools.makeImageFromDebDist {
    inherit (vmTools.debDistros.ubuntu2004x86_64) name fullName urlPrefix packagesLists;
    packages =
      lib.filter (p:
        !lib.elem p [
          "g++"
          "make"
          "dpkg-dev"
          "pkg-config"
          "sysvinit"
        ])
      vmTools.debDistros.ubuntu2004x86_64.packages
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
      ];
    size = 5120; # 5GB in MB
    createRootFS = ''
      disk=/dev/vda
      ${gptfdisk}/bin/sgdisk $disk \
        -n1:0:+100M -t1:ef00 -c1:esp \
        -n2:0:0 -t2:8300 -c2:root
      ${util-linux}/bin/partx -u "$disk"
      ${dosfstools}/bin/mkfs.vfat -F32 -n ESP "$disk"1
      part="$disk"2
      ${e2fsprogs}/bin/mkfs.ext4 "$part" -L root
      mkdir /mnt
      ${util-linux}/bin/mount -t ext4 "$part" /mnt
      mkdir -p /mnt/{proc,dev,sys,boot/efi}
      ${util-linux}/bin/mount -t vfat "$disk"1 /mnt/boot/efi
      touch /mnt/.debug
    '';
    postInstall = ''
      ${util-linux}/bin/mount -t proc proc /mnt/proc
      ${util-linux}/bin/mount -t sysfs sysfs /mnt/sys
      ${util-linux}/bin/mount -o bind /dev /mnt/dev
      ${util-linux}/bin/mount -o bind /dev/pts /mnt/dev/pts

      chroot /mnt /bin/bash -exuo pipefail <<CHROOT
      export PATH=/usr/sbin:/usr/bin:/sbin:/bin
      echo LABEL=root / ext4 defaults > /etc/fstab
      update-initramfs -k all -c
      cat >> /etc/default/grub <<EOF
      GRUB_TIMEOUT=5
      GRUB_CMDLINE_LINUX="console=ttyS0"
      GRUB_CMDLINE_LINUX_DEFAULT=""
      EOF
      sed -i '/TIMEOUT_HIDDEN/d' /etc/default/grub
      update-grub
      grub-install --target x86_64-efi
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
      # Configure SSH
      rm /etc/ssh/ssh_host_*
      cat > /etc/systemd/system/generate-host-keys.service <<SERVICE
      [Install]
      WantedBy=ssh.service
      [Unit]
      Before=ssh.service
      [Service]
      ExecStart=dpkg-reconfigure openssh-server
      SERVICE
      systemctl enable generate-host-keys
      # Set root password (you might want to change this)
      echo root:root | chpasswd
      # Add your public SSH key here
      mkdir -p /root/.ssh
      chmod 0700 /root
      cat >/root/.ssh/authorized_keys <<KEYS
      ssh-ed25519 YOUR_PUBLIC_SSH_KEY
      KEYS
      CHROOT

      ${util-linux}/bin/umount /mnt/dev/pts
      ${util-linux}/bin/umount /mnt/dev
      ${util-linux}/bin/umount /mnt/sys
      ${util-linux}/bin/umount /mnt/proc
      ${util-linux}/bin/umount /mnt/boot/efi
    '';
  };

  treadmillStore = derivation {
    name = "treadmill-store";
    system = builtins.currentSystem;
    builder = "${bash}/bin/bash";
    args = [
      "-c"
      ''
        set -euo pipefail

        # Create the base directory structure
        ${coreutils}/bin/mkdir -p $out/dev-image-store/blobs
        ${coreutils}/bin/mkdir -p $out/dev-image-store/images

        # Calculate the SHA256 hash of the qcow2 file
        BLOB_HASH=$(${coreutils}/bin/sha256sum ${ubuntuImage}/disk-image.qcow2 | ${coreutils}/bin/cut -d' ' -f1)

        # Create the blob directory structure and copy the qcow2 file
        ${coreutils}/bin/mkdir -p $out/dev-image-store/blobs/''${BLOB_HASH:0:2}/''${BLOB_HASH:2:2}/''${BLOB_HASH:4:2}
        ${coreutils}/bin/cp ${ubuntuImage}/disk-image.qcow2 $out/dev-image-store/blobs/''${BLOB_HASH:0:2}/''${BLOB_HASH:2:2}/''${BLOB_HASH:4:2}/$BLOB_HASH

        # Create the image-specific manifest file
        ${coreutils}/bin/cat > $out/dev-image-store/image_manifest.toml << EOF
        manifest_version = "0.0"
        manifest_extensions = [ "org.tockos.treadmill.manifest-ext.base" ]

        "org.tockos.treadmill.manifest-ext.base.label" = "Ubuntu 20.04 base installation"
        "org.tockos.treadmill.manifest-ext.base.revision" = 0
        "org.tockos.treadmill.manifest-ext.base.description" = '''
        Base Ubuntu 20.04 installation, without any customizations.
        Minimal packages selected, DHCP network configuration.
        Credentials: root / root
        '''

        ["org.tockos.treadmill.manifest-ext.base.attrs"]
        "org.tockos.treadmill.image.qemu_layered_v0.head" = "layer-0"

        ["org.tockos.treadmill.manifest-ext.base.blobs".layer-0]
        "org.tockos.treadmill.manifest-ext.base.sha256-digest" = "$BLOB_HASH"
        "org.tockos.treadmill.manifest-ext.base.size" = $(${coreutils}/bin/stat -c%s ${ubuntuImage}/disk-image.qcow2)

        ["org.tockos.treadmill.manifest-ext.base.blobs".layer-0."org.tockos.treadmill.manifest-ext.base.attrs"]
        "org.tockos.treadmill.image.qemu_layered_v0.blob-virtual-size" = "5368709120"
        EOF

        # Calculate the SHA256 hash of the image-specific manifest file
        IMAGE_HASH=$(${coreutils}/bin/sha256sum $out/dev-image-store/image_manifest.toml | ${coreutils}/bin/cut -d' ' -f1)

        # Create the image-specific directory and move the manifest
        ${coreutils}/bin/mkdir -p $out/dev-image-store/images/''${IMAGE_HASH:0:2}/''${IMAGE_HASH:2:2}/''${IMAGE_HASH:4:2}
        ${coreutils}/bin/mv $out/dev-image-store/image_manifest.toml $out/dev-image-store/images/''${IMAGE_HASH:0:2}/''${IMAGE_HASH:2:2}/''${IMAGE_HASH:4:2}/$IMAGE_HASH

        # Create the manifest.toml file in the root of dev-image-store
        ${coreutils}/bin/cat > $out/dev-image-store/manifest.toml << EOF
        manifest_version = "0.0"
        manifest_extensions = [ "org.tockos.treadmill.manifest-ext.base" ]

        "org.tockos.treadmill.manifest-ext.base.label" = "Ubuntu 20.04 base installation"
        "org.tockos.treadmill.manifest-ext.base.revision" = 0
        "org.tockos.treadmill.manifest-ext.base.description" = '''
        Base Ubuntu 20.04 installation, without any customizations.
        Minimal packages selected, DHCP network configuration.
        Credentials: root / root
        '''

        ["org.tockos.treadmill.manifest-ext.base.attrs"]
        "org.tockos.treadmill.image.qemu_layered_v0.head" = "layer-0"

        ["org.tockos.treadmill.manifest-ext.base.blobs".layer-0]
        "org.tockos.treadmill.manifest-ext.base.sha256-digest" = "$BLOB_HASH"
        "org.tockos.treadmill.manifest-ext.base.size" = $(${coreutils}/bin/stat -c%s ${ubuntuImage}/disk-image.qcow2)

        ["org.tockos.treadmill.manifest-ext.base.blobs".layer-0."org.tockos.treadmill.manifest-ext.base.attrs"]
        "org.tockos.treadmill.image.qemu_layered_v0.blob-virtual-size" = 5368709120
        EOF

        # Verify the directory structure
        ${tree}/bin/tree $out/dev-image-store/

        # Output the IMAGE_HASH for future reference
        ${coreutils}/bin/echo "IMAGE_HASH: $IMAGE_HASH" > $out/image_hash.txt
      ''
    ];
    buildInputs = [coreutils tree];
  };
in {
  inherit ubuntuImage treadmillStore;
}
