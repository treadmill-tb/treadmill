#!/bin/sh
# Shared in-guest provisioning for Treadmill images.
#
# Runs under `virt-customize --run`: the target root is mounted and the guest
# kernel is NEVER booted (no chroot/`/mnt` prefix, no running systemd). Ports
# the Treadmill-specific steps from the legacy images/ubuntu-2204/rootfs.nix
# `postInstall` chroot. See doc/images-libguestfs-build-plan.md §6.
#
# POSIX sh (the guest shell may be dash): no bash arrays / `[[ ]]`.
set -eu

# Manifest-derived values handed in by build-image.sh (the host environment is
# not forwarded across virt-customize --run): puppet_daemon_args, serial_consoles.
# shellcheck source=/dev/null
. /tmp/provision.env

# --- treadmill user: passwordless sudo ------------------------------------
useradd -m -u 1000 -s /bin/bash tml
usermod -a -G plugdev tml
usermod -a -G tty tml
echo "tml ALL=(ALL) NOPASSWD: ALL" >/etc/sudoers.d/010_tml-nopasswd
chmod 440 /etc/sudoers.d/010_tml-nopasswd

# --- USB device access for the tml user (uaccess) -------------------------
cat >/etc/udev/rules.d/99-tml.rules <<'RULES'
SUBSYSTEM=="usb", GROUP="plugdev", TAG+="uaccess"
RULES

# --- allow the puppet daemon to own its D-Bus name ------------------------
cat >/etc/dbus-1/system.d/ci.treadmill.Puppet.conf <<'DBUSCONF'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <policy context="default">
    <allow own="ci.treadmill.Puppet"/>
    <allow send_destination="ci.treadmill.Puppet"/>
    <allow receive_sender="ci.treadmill.Puppet"/>
  </policy>
</busconfig>
DBUSCONF

# --- treadmill puppet daemon ----------------------------------------------
# Type=notify: only report started once connected to the supervisor.
# NotifyAccess=main: ignore status updates from child processes.
# ExecStart is assembled from the per-image $puppet_daemon_args (expanded here,
# when the unit is written). Wrapped in `bash -c 'exec …'` so transports whose
# args need runtime evaluation work — e.g. the netboot RPi passes
# `--tcp-control-socket-addr "$(ip route …)"`, which must be resolved at service
# start, not at build time (the unquoted heredoc leaves the `$(…)` literal in
# the unit). `exec` keeps tml-puppet as the main PID, so Type=notify/main still
# sees its sd_notify. NB: $puppet_daemon_args must not contain a single quote.
cat >/etc/systemd/system/tml-puppet.service <<SERVICE
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
ExecStart=/bin/bash -c 'exec /usr/local/bin/tml-puppet daemon ${puppet_daemon_args} --authorized-keys-file /home/tml/.ssh/authorized_keys --exit-on-authorized-keys-update-error --job-info-dir /run/tml --parameters-dir /run/tml/parameters'
Restart=always
RestartSec=5s
SERVICE
systemctl enable tml-puppet.service

# --- rustup (installed for the tml user; no default toolchain) ------------
chmod +x /opt/rustup-init
sudo -u tml -H /opt/rustup-init -y --default-toolchain none --profile minimal

# --- runtime root growth (expandroot, once per fresh deployment) ----------
# The image ships a small virtual disk; the host enlarges it at deploy time and
# this grows the root partition + filesystem to fill it on first boot.
chmod +x /opt/expandroot.sh
touch /firstboot-expandroot
cat >/etc/systemd/system/firstboot-expandroot.service <<'SERVICE'
[Install]
WantedBy=multi-user.target
[Unit]
ConditionPathExists=/firstboot-expandroot
[Service]
Type=simple
ExecStart=/opt/expandroot.sh
ExecStartPost=/bin/rm /firstboot-expandroot
SERVICE
systemctl enable firstboot-expandroot.service

# --- unique SSH host keys per deployment ----------------------------------
# Remove the baked-in keys and regenerate them before ssh.service starts.
# openssh-server is preinstalled in the cloud image, so there is no install-time
# start race (the legacy policy-rc.d dance is gone).
rm -f /etc/ssh/ssh_host_*_key /etc/ssh/ssh_host_*_key.pub
cat >/etc/systemd/system/ssh-generate-host-keys.service <<'SERVICE'
[Install]
WantedBy=multi-user.target
[Unit]
Before=ssh.service
ConditionPathExistsGlob=!/etc/ssh/ssh_host_*_key
[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A
RemainAfterExit=true
SERVICE
systemctl enable ssh-generate-host-keys.service

# --- serial console autologin for the tml user ----------------------------
for dev in $serial_consoles; do
	mkdir -p "/etc/systemd/system/serial-getty@${dev}.service.d"
	cat >"/etc/systemd/system/serial-getty@${dev}.service.d/override.conf" <<'OVERRIDE'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin tml --noclear %I
OVERRIDE
done

# --- networking: systemd-networkd + resolved, DHCP + IPv6 SLAAC -----------
# Privacy extensions off (stable addresses for a testbed). The RPi NIC is `eth0`
# via net.ifnames=0, hence the en*/eth* match.
mkdir -p /etc/systemd/network
cat >/etc/systemd/network/10-eth.network <<'NETWORK'
[Match]
Name=en* eth*
[Network]
DHCP=yes
IPv6AcceptRA=yes
IPv6PrivacyExtensions=no
[Link]
RequiredForOnline=true
NETWORK
ln -snf /run/systemd/resolve/stub-resolv.conf /etc/resolv.conf
systemctl enable systemd-networkd systemd-resolved
