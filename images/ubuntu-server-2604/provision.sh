#!/bin/sh
# Ubuntu-specific in-guest provisioning.
#
# Runs under `virt-customize --run` AFTER images/lib/provision-common.sh. See
# doc/images-libguestfs-build-plan.md §7. Partitioning / UEFI / grub-install /
# initramfs all come from the cloud image and are NOT reapplied here.
set -eu

# cloud-init is purged: we own growpart (expandroot), ssh host keys, and
# networking (systemd-networkd, via provision-common.sh) with deterministic
# units. Remove the netplan config it would otherwise drive the network from.
apt-get purge -y cloud-init
rm -f /etc/netplan/*.yaml

# grub serial console: mirror kernel + bootloader output to ttyS0 (no monitor
# attached) and drop `quiet` so boot is verbose. /etc/default/grub is sourced by
# update-grub, so appended assignments win over the cloud image's defaults.
cat >>/etc/default/grub <<'GRUB'

# Treadmill: serial console, verbose boot.
GRUB_TIMEOUT=5
GRUB_CMDLINE_LINUX="console=ttyS0"
GRUB_CMDLINE_LINUX_DEFAULT=""
GRUB_TERMINAL="serial"
GRUB
# Show the menu/timeout on the serial console instead of hiding it.
sed -i '/GRUB_TIMEOUT_STYLE/d;/GRUB_HIDDEN_TIMEOUT/d' /etc/default/grub
update-grub

# --- web terminal ---------------------------------------------------------
# ttyd serves a terminal over HTTP/WebSocket on the port a job's services are
# reached at, and logs straight into the tml user (whose shell has passwordless
# sudo). Nothing in the job authenticates the request yet: the port is only ever
# as private as the network the job sits on.
#
# --ipv6 makes libwebsockets bind dual-stack: a job is reached at its routable
# IPv6 address, while a NATed IPv4 one still works for local testing.
cat >/etc/systemd/system/ttyd.service <<'SERVICE'
[Install]
WantedBy=multi-user.target
[Unit]
After=network.target
[Service]
ExecStart=/usr/bin/ttyd --port 3860 --ipv6 --writable /bin/bash --login
User=tml
Group=tml
Restart=always
RestartSec=5s
SERVICE
systemctl enable ttyd.service

# The puppet announces this to the switchboard at boot (and on reload), which is
# what makes the terminal addressable as `webterm-<job-id>` at a gateway.
cat >/etc/tml/services.d/webterm.json <<'SERVICEDECL'
{
	"name": "webterm",
	"label": "Terminal",
	"protocol": "webapp"
}
SERVICEDECL
