#!/bin/sh
# webide overlay: coder's code-server, announced as the job's `webide` service.
#
# Runs on a delta backed by the image's base delta, so the tml user, the puppet
# and the caddy from images/lib/provision-common.sh already exist. Shared by
# every image family; build-image.sh has copied the arch-matched .deb to
# /var/tmp.
set -eu

# The .deb declares no dependencies (it bundles its own node), so this installs
# offline.
dpkg -i /var/tmp/code-server.deb
rm -f /var/tmp/code-server.deb

# --auth none: the socket is only reachable through the job's caddy, which first
# validates a gateway-issued JWT for this job's `webide` audience. The 0750
# runtime directory is what keeps other users in the job out.
cat >/etc/systemd/system/code-server.service <<'SERVICE'
[Install]
WantedBy=multi-user.target
[Unit]
After=network.target
[Service]
User=tml
Group=tml
RuntimeDirectory=tml-code-server
RuntimeDirectoryMode=0750
ExecStart=/usr/bin/code-server --auth none --socket /run/tml-code-server/code-server.sock --socket-mode 0600 --disable-telemetry --disable-update-check --disable-workspace-trust /home/tml
Restart=always
RestartSec=5s
SERVICE
systemctl enable code-server.service

# The puppet announces this, which is what makes the IDE addressable as
# `webide-<job-id>` at a gateway.
cat >/etc/tml/services.d/webide.json <<'SERVICEDECL'
{
	"name": "webide",
	"label": "Web IDE",
	"protocol": "webapp",
	"upstream": "unix//run/tml-code-server/code-server.sock"
}
SERVICEDECL
