#!/usr/bin/env bash

# Run a Treadmill supervisor standalone — no switchboard, no Postgres, no NATS.
#
# This brings up a per-developer zot registry, copies an image into it from an
# upstream registry (ghcr.io by default), resolves the requested tag to its OCI
# manifest digest, and runs the supervisor under the switchboard-less `local`
# connector (see connector/local) to boot exactly one job. The guest console is
# streamed straight to this terminal; Ctrl-C asks the supervisor to stop the job
# and tear down, then this script stops the registry.
#
# It is intended for local development/testing. For the full control-plane stack
# (switchboard + console + scheduler) use `nix run .#devstack` instead.
#
# Nix injects the supervisor binary and firmware blob paths via the environment
# (see nix/apps.nix); run it through `nix run .#qemu-supervisor-local`.

set -euo pipefail

# ---------------------------------------------------------------------------
# Inputs injected from Nix (nix/apps.nix). The supervisor binary defaults to
# whatever is on PATH so the script also works inside a dev shell.
# ---------------------------------------------------------------------------
supervisor_bin="${TML_SUPERVISOR_BIN:-treadmill-qemu-supervisor}"
ovmf_code="${TML_OVMF_CODE:-}"     # x86_64 UEFI code blob (read-only)
ovmf_vars="${TML_OVMF_VARS:-}"     # x86_64 UEFI vars template
uefi_code="${TML_UEFI_CODE:-}"   # aarch64 UEFI code blob (read-only)
uefi_vars="${TML_UEFI_VARS:-}"   # aarch64 UEFI vars template

# ---------------------------------------------------------------------------
# Defaults, overridable by flags / environment.
# ---------------------------------------------------------------------------
arch="x86_64"
use_kvm="auto"
keep_state=0
mem="4G"
disk_max_bytes="34359738368"  # 32 GiB ceiling for the per-job overlay
image=""
ssh_keys=()
params=()
extra_qemu_args=()
stop_after=""

state_dir="${TML_LOCAL_SUP_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/treadmill-local-sup}"
zot_port="${TML_ZOT_PORT:-5050}"

usage() {
  cat <<USAGE
Usage: nix run .#qemu-supervisor-local -- --image <registry>/<repo>:<tag> [options]

Required:
  --image REF            Image to run, e.g. ghcr.io/org/repo:tag (tag resolved
                         to a manifest digest automatically).

Options:
  --arch {x86_64|aarch64}  Guest architecture (default: x86_64).
  --no-kvm               Force TCG even when /dev/kvm is available.
  --mem SIZE             Guest RAM (default: $mem).
  --disk-max-bytes N     Per-job overlay ceiling (default: $disk_max_bytes).
  --ssh-key KEY          SSH public key to deploy (repeatable).
  -p, --param KEY=VALUE  Job parameter (repeatable).
  --stop-after DURATION  Auto-stop the job after e.g. 5m (default: run until
                         the guest exits or Ctrl-C).
  --qemu-arg ARG         Extra raw qemu argument (repeatable).
  --keep                 Do not wipe the state directory on exit.
  -h, --help             Show this help.

State (registry store, configs, per-job overlays) lives under:
  $state_dir
USAGE
}

# ---------------------------------------------------------------------------
# Argument parsing.
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --image)          image="$2"; shift 2 ;;
    --arch)           arch="$2"; shift 2 ;;
    --no-kvm)         use_kvm="no"; shift ;;
    --mem)            mem="$2"; shift 2 ;;
    --disk-max-bytes) disk_max_bytes="$2"; shift 2 ;;
    --ssh-key)        ssh_keys+=("$2"); shift 2 ;;
    -p|--param)       params+=("$2"); shift 2 ;;
    --stop-after)     stop_after="$2"; shift 2 ;;
    --qemu-arg)       extra_qemu_args+=("$2"); shift 2 ;;
    --keep)           keep_state=1; shift ;;
    -h|--help)        usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [ -z "$image" ]; then
  echo "Error: --image is required." >&2
  usage >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# Parse the image reference into (registry host, repository, reference).
# Accepts <host>/<path>:<tag> or <host>/<path>@sha256:<hex>. A registry host is
# the first path segment and must look like one (contains '.' or ':', or is
# 'localhost'); we require it so the upstream to copy from is unambiguous.
# ---------------------------------------------------------------------------
first_seg="${image%%/*}"
case "$first_seg" in
  *.*|*:*|localhost) ;;
  *)
    echo "Error: --image must include a registry host, e.g. ghcr.io/org/repo:tag" >&2
    exit 2 ;;
esac
registry_host="$first_seg"
remainder="${image#*/}"   # <repo>[:<tag> | @sha256:<hex>]

if [[ "$remainder" == *@* ]]; then
  repo="${remainder%@*}"
  ref="${remainder#*@}"     # sha256:<hex>
else
  # Split on the LAST ':' to allow ':' inside a (hypothetical) port-less repo.
  if [[ "$remainder" == *:* ]]; then
    repo="${remainder%:*}"
    ref="${remainder##*:}"
  else
    repo="$remainder"
    ref="latest"
  fi
fi

# ---------------------------------------------------------------------------
# Resolve per-arch firmware + qemu binary, and decide on acceleration.
# ---------------------------------------------------------------------------
host_arch="$(uname -m)"
case "$arch" in
  x86_64)
    qemu_binary="qemu-system-x86_64"
    fw_code="$ovmf_code"
    fw_vars="$ovmf_vars"
    machine="q35"
    blk_device="virtio-blk-pci"
    net_device="virtio-net-pci"
    tcg_cpu="max"
    ;;
  aarch64)
    qemu_binary="qemu-system-aarch64"
    fw_code="$uefi_code"
    fw_vars="$uefi_vars"
    machine="virt"
    blk_device="virtio-blk-device"
    net_device="virtio-net-device"
    tcg_cpu="cortex-a57"
    ;;
  *)
    echo "Error: unsupported --arch $arch (expected x86_64 or aarch64)." >&2
    exit 2 ;;
esac

if [ -z "$fw_code" ] || [ -z "$fw_vars" ]; then
  echo "Error: UEFI firmware for $arch was not provided (run via nix run)." >&2
  exit 1
fi

accel=""
cpu="$tcg_cpu"
if [ "$use_kvm" != "no" ] && [ "$arch" = "$host_arch" ] && [ -r /dev/kvm ]; then
  accel=",accel=kvm"
  cpu="host"
fi

# ---------------------------------------------------------------------------
# Lay out the state directory.
# ---------------------------------------------------------------------------
umask 077
zot_dir="$state_dir/zot"
cfg_dir="$state_dir/config"
sup_state_dir="$state_dir/supervisor"
mkdir -p "$zot_dir" "$cfg_dir" "$sup_state_dir"

# A stable supervisor id for this state dir (persisted across runs).
sup_id_file="$state_dir/supervisor_id"
if [ ! -f "$sup_id_file" ]; then
  cat /proc/sys/kernel/random/uuid > "$sup_id_file"
fi
supervisor_id="$(cat "$sup_id_file")"

zot_authority="127.0.0.1:$zot_port"

# ---------------------------------------------------------------------------
# Render the zot config: a plain local store we push into with skopeo.
# ---------------------------------------------------------------------------
cat > "$cfg_dir/zot.json" <<JSON
{"storage":{"rootDirectory":"$zot_dir","dedupe":true},
 "http":{"address":"127.0.0.1","port":"$zot_port"},
 "log":{"level":"warn"}}
JSON

# ---------------------------------------------------------------------------
# Per-job start script: copy the writable UEFI vars template into the job
# workdir (the read-only code blob stays shared). Mirrors the OVMF start-script
# pattern used elsewhere in the repo.
# ---------------------------------------------------------------------------
vars_start="$cfg_dir/uefi-vars-start.sh"
cat > "$vars_start" <<SH
#!/usr/bin/env bash
set -euo pipefail
cp "$fw_vars" "\$TML_JOB_WORKDIR/UEFI_VARS.fd"
chmod u+w "\$TML_JOB_WORKDIR/UEFI_VARS.fd"
SH
chmod +x "$vars_start"

# ---------------------------------------------------------------------------
# Render the static supervisor config. Per-job inputs (image digest, repo, ssh
# keys, parameters) are NOT here — they are passed as command-line flags to the
# `local` connector below. The control socket listens on loopback; the guest
# reaches it via 10.0.2.2 (the host address in qemu SLIRP networking).
# ---------------------------------------------------------------------------
sup_cfg="$cfg_dir/supervisor.toml"
{
  cat <<TOML
[base]
supervisor_id = "$supervisor_id"
coord_connector = "local"

[oci_store]
registry = "$zot_authority"
store_root = "$zot_dir"

[qemu]
qemu_binary = "$qemu_binary"
qemu_img_binary = "qemu-img"
state_dir = "$sup_state_dir"
working_disk_max_bytes = $disk_max_bytes
tcp_control_socket_listen_addr = "127.0.0.1:3859"
start_script = "$vars_start"
qemu_args = [
  "-name", "tml-{job_id}",
  "-nographic",
  "-machine", "$machine$accel",
  "-cpu", "$cpu",
  "-m", "$mem",
  "-drive", "if=pflash,format=raw,readonly=on,file=$fw_code",
  "-drive", "if=pflash,format=raw,file={job_workdir}/UEFI_VARS.fd",
  "-device", "$blk_device,drive={disk_node}",
  "-netdev", "user,id=net0,hostfwd=tcp::2222-:22",
  "-device", "$net_device,netdev=net0,id=nic0",
  "-fw_cfg", "name=opt/org.tockos.treadmill.tcp-ctrl-socket,string=10.0.2.2:3859",
TOML
  for a in "${extra_qemu_args[@]}"; do
    printf '  "%s",\n' "$a"
  done
  echo "]"
} > "$sup_cfg"

# ---------------------------------------------------------------------------
# Teardown: stop the registry and (unless --keep) wipe the state dir.
# ---------------------------------------------------------------------------
zot_pid=""
cleanup() {
  trap - EXIT
  if [ -n "$zot_pid" ]; then
    kill "$zot_pid" 2>/dev/null || true
    wait "$zot_pid" 2>/dev/null || true
  fi
  if [ "$keep_state" != 1 ]; then
    rm -rf "$state_dir"
  fi
}
trap cleanup EXIT

wait_http() {
  local url="$1" tries="${2:-120}"
  for _ in $(seq 1 "$tries"); do
    if curl -fsS -o /dev/null "$url" 2>/dev/null; then return 0; fi
    sleep 0.5
  done
  return 1
}

# ---------------------------------------------------------------------------
# Start the registry and make the image available, then resolve its digest.
# ---------------------------------------------------------------------------
echo "Starting zot registry (http://$zot_authority)"
zot serve "$cfg_dir/zot.json" >"$cfg_dir/zot.log" 2>&1 &
zot_pid="$!"
if ! wait_http "http://$zot_authority/v2/" 120; then
  echo "zot did not become ready; see $cfg_dir/zot.log" >&2
  exit 1
fi

echo "Copying $registry_host/$repo:$ref into the local registry"
skopeo \
  --registries-conf /dev/null \
  --insecure-policy copy \
  --dest-tls-verify=false \
  "docker://$registry_host/$repo:$ref" \
  "docker://$zot_authority/$repo:$ref"

# `skopeo inspect` against the local registry resolves the manifest digest.
echo "Resolving $repo:$ref to a manifest digest"
manifest_digest="$(\
  skopeo inspect \
    --raw \
    --registries-conf /dev/null \
    --tls-verify=false \
    "docker://$zot_authority/$repo:$ref" \
  | sha256sum \
  | awk '{ print "sha256:" $1; }' \
)"
echo "$manifest_digest"
if [ -z "$manifest_digest" ]; then
  echo "Failed to resolve a manifest digest for $repo:$ref." >&2
  exit 1
fi
echo "  $repo:$ref => $manifest_digest"

# ---------------------------------------------------------------------------
# Assemble the per-job CLI flags for the `local` connector and run it.
# ---------------------------------------------------------------------------
job_args=(--manifest-digest "$manifest_digest" --repository "$repo")
for k in "${ssh_keys[@]}"; do job_args+=(--ssh-key "$k"); done
for p in "${params[@]}"; do job_args+=(--param "$p"); done
if [ -n "$stop_after" ]; then job_args+=(--stop-after "$stop_after"); fi

cat <<EOF

============================================================
  treadmill standalone supervisor
    image          : $registry_host/$repo:$ref
    digest         : $manifest_digest
    arch           : $arch ($([ -n "$accel" ] && echo KVM || echo TCG))
    registry       : http://$zot_authority
    state          : $state_dir
    ssh into guest : ssh -p 2222 <user>@127.0.0.1   (once it boots)
  (Ctrl-C stops the job and tears the stack down.)
============================================================

EOF

# Run in the foreground so the guest console streams here and Ctrl-C reaches the
# supervisor's graceful-shutdown handler. cleanup (EXIT trap) stops zot after.
"$supervisor_bin" -c "$sup_cfg" "${job_args[@]}"
