#!/bin/sh
# Raspberry Pi OS gha-runner overlay: the GitHub Actions self-hosted runner
# service (aarch64 == the arm64 runner release).
#
# Runs under `virt-customize --run` on a delta backed by the raspberrypios-13
# BASE root delta (so provision-common.sh / the tml user / puppet already exist;
# this overlay only adds the runner units). The guest kernel is NEVER booted.
# See doc/images-libguestfs-build-plan.md §7. Mirror of
# images/ubuntu-server-2604/gha-runner/provision.sh; the only difference is
# RUNNER_ARCH (the runner is still downloaded at first boot, not pinned).
#
# build-image.sh has already --copy-in'd images/lib/install-gh-actions-runner.sh
# to /opt. POSIX sh (the guest shell may be dash): no bash arrays / `[[ ]]`.
set -eu

chmod +x /opt/install-gh-actions-runner.sh

# --- first-boot: download + install the latest runner release -------------
# A oneshot run before gh-actions-runner.service; RUNNER_ARCH is this image's
# release slug (arm64 == aarch64). The runner is fetched at deploy time, never
# baked into the image (see install-gh-actions-runner.sh).
cat >/etc/systemd/system/install-gh-actions-runner.service <<'SERVICE'
[Unit]
Description=Download and install latest GitHub Actions Runner release
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=true
ExecStart=/opt/install-gh-actions-runner.sh
Restart=on-failure
User=root
Group=root
Environment=RUNNER_ARCH=arm64
Environment=RUNNER_OWNER=1000
Environment=RUNNER_GROUP=1000

[Install]
WantedBy=multi-user.target
SERVICE
systemctl enable install-gh-actions-runner.service

# --- the runner service ---------------------------------------------------
# Configures (unless a JIT config / existing credentials are present) and runs
# the runner as the tml user. The job parameters come from the supervisor under
# /run/tml at runtime. Quoted heredoc: $REPO_URL / $(cat ...) stay literal for
# the in-unit shells.
cat >/etc/systemd/system/gh-actions-runner.service <<'SERVICE'
[Unit]
Description=GitHub Actions Runner
After=network.target tml-puppet.service install-gh-actions-runner.service
Wants=tml-puppet.service install-gh-actions-runner.service

[Service]
ExecStartPre=/bin/bash -Eexuo pipefail -c 'cp /opt/gh-actions-runner/bin/runsvc.sh /opt/gh-actions-runner/runsvc.sh && chown tml:tml /opt/gh-actions-runner/runsvc.sh && if [ -f /opt/gh-actions-runner/.credentials ]; then exit 0; fi && if [ -f /run/tml/parameters/gh-actions-runner-encoded-jit-config ]; then exit 0; fi && REPO_URL=$(cat /run/tml/parameters/gh-actions-runner-repo-url) && RUNNER_TOKEN=$(cat /run/tml/parameters/gh-actions-runner-token) && JOB_ID=$(cat /run/tml/job-id) && /opt/gh-actions-runner/config.sh --url $REPO_URL --token $RUNNER_TOKEN --name tml-gh-actions-runner-$JOB_ID --labels tml-gh-actions-runner-$JOB_ID --unattended --ephemeral'
ExecStartPre=-+/bin/bash /run/tml/parameters/gh-actions-runner-exec-start-pre-sh
ExecStart=/bin/bash -Eeuo pipefail -c 'if [ -f /run/tml/parameters/gh-actions-runner-encoded-jit-config ]; then echo "Starting GitHub Actions Runner from JIT config"; /opt/gh-actions-runner/run.sh --jitconfig $(cat /run/tml/parameters/gh-actions-runner-encoded-jit-config); else echo "Starting preconfigured GitHub Actions Runner"; /opt/gh-actions-runner/run.sh; fi'
Restart=on-failure
# KillSignal=SIGINT for run.sh (a compatible stop signal to the run.sh above).
KillSignal=SIGINT
TimeoutStopSec=5m
User=tml
Group=tml
WorkingDirectory=/opt/gh-actions-runner
ExecStopPost=-+/bin/bash /run/tml/parameters/gh-actions-runner-exec-stop-post-sh

[Install]
WantedBy=multi-user.target
SERVICE
systemctl enable gh-actions-runner.service

# --- serial-console login drops the operator into the live journal --------
# The runner owns the foreground; give the autologin tml shell a tail of the
# system journal (then fall back to a normal shell).
cat >/opt/journal-login-shell <<'SCRIPT'
#!/bin/bash
exec /bin/bash --init-file <(echo 'sudo journalctl -f; . "$HOME/.bashrc"')
SCRIPT
chmod +x /opt/journal-login-shell
sed -i -E 's|^(tml:.*):/bin/bash$|\1:/opt/journal-login-shell|' /etc/passwd
