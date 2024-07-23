{
  lib,
  vmTools,
  udev,
  gptfdisk,
  util-linux,
  dosfstools,
  e2fsprogs,
  systemd,
}:
vmTools.makeImageFromDebDist {
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
    # Place your config files here
    # For example:
    # cat > /etc/myconfig.conf <<CONFIG
    # your_config_content_here
    # CONFIG
    # Place your public binary here
    # For example:
    # cp ./custom_binary /usr/local/bin/
    # chmod +x /usr/local/bin/custom_binary
    CHROOT

    ${util-linux}/bin/umount /mnt/dev/pts
    ${util-linux}/bin/umount /mnt/dev
    ${util-linux}/bin/umount /mnt/sys
    ${util-linux}/bin/umount /mnt/proc
    ${util-linux}/bin/umount /mnt/boot/efi
  '';
}
