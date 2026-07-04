#!/usr/bin/env bash

set -euo pipefail

# Config files hold the OAuth client secret; keep them private.
umask 077

# Config variables and sensible defaults:
state_dir="${TML_DEVSTACK_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/treadmill-devstack}"
sb_port="${TML_SB_PORT:-8000}"
console_port="${TML_CONSOLE_PORT:-8080}"
pg_port="${TML_PG_PORT:-5432}"
nats_port="${TML_NATS_PORT:-4222}"
nats_ws_port="${TML_NATS_WS_PORT:-4223}"
zot_port="${TML_ZOT_PORT:-5000}"
user="$(id -un)"

# Linux-only registry/supervisor bootstrap inputs, injected from Nix.
# Empty on non-Linux (the tiny-efi fixture isn't built there): the zot +
# qemu-supervisor stack is then skipped and `.#dev` is just the
# switchboard/console/NATS stack, as before.
fixture_layout="${TML_FIXTURE_LAYOUT:-}"
aavmf_code="${TML_AAVMF_CODE:-}"
aavmf_vars="${TML_AAVMF_VARS:-}"
if [ -n "$fixture_layout" ]; then
    enable_supervisor=1
else
    enable_supervisor=0
fi

# Stable dev identities. The host auth_token bearer matches the supervisor
# ws_connector token; the API token bearer drives the smoke stage. 'alice' is
# the built-in mock admin identity.
host_id="7d55ec6d-15e7-4b84-8c04-7c085fe60df4"
dev_user_id="a11ce5a1-0000-4000-8000-000000000001"
admins_group_id="00000000-0000-0000-0000-000000000001"
host_token_bearer="OCkrhbDMiUG7rY1LlSfywBvgkqb1CyOt0djIgos9QDw="
api_token_bearer="B1oy2ko1wVdGKbvKc/9dKi7ggZYLTLzdm2As4CWV15c="

pg_dir="$state_dir/pg"
sock_dir="$state_dir/pg-sockets"
cfg_dir="$state_dir/config"
log_dir="$state_dir/logs"
zot_dir="$state_dir/zot"
sup_state_dir="$state_dir/supervisor"
mkdir -p "$state_dir" "$sock_dir" "$cfg_dir" "$log_dir"

# Poll an HTTP endpoint until it answers (daemon readiness).
wait_http() {
  local url="$1" tries="${2:-60}"
  for _ in $(seq 1 "$tries"); do
    if curl -fsS -o /dev/null "$url" 2>/dev/null; then return 0; fi
    sleep 0.5
  done
  return 1
}

# GitHub login is enabled only when both credentials are present in the
# environment. The secret is never written to the config file: it is
# passed to switchboard via TML_OAUTH__GITHUB__* env vars (figment reads
# TML_-prefixed env, nesting on `__`), leaving only the non-secret
# redirect URLs on disk.
if [ -n "${TML_DEV_GITHUB_CLIENT_ID:-}" ] \
   && [ -n "${TML_DEV_GITHUB_CLIENT_SECRET:-}" ]; then
  oauth_enabled=1
else
  oauth_enabled=0
fi

# --- Postgres: init once, (re)start, ensure the database exists -------
if [ ! -f "$pg_dir/PG_VERSION" ]; then
  echo "Initialising PostgreSQL cluster in $pg_dir"
  initdb -D "$pg_dir" -U "$user" --auth=trust >/dev/null
fi

if ! pg_ctl -D "$pg_dir" status >/dev/null 2>&1; then
  echo "Starting PostgreSQL (unix socket in $sock_dir, port $pg_port)"
  # An empty -h argument disables the TCP listener so clients connect
  # over the unix socket only (the triple-quote below is the Nix
  # escape that emits an empty single-quoted shell argument).
  pg_ctl -D "$pg_dir" -l "$log_dir/postgres.log" -w \
    -o "-h '' --unix_socket_directories='$sock_dir' -p $pg_port" start
  started_pg=1
else
  started_pg=0
fi

if ! psql -h "$sock_dir" -p "$pg_port" -U "$user" -d postgres -lqt \
    | cut -d '|' -f1 | grep -qw tml_switchboard; then
  echo "Creating database tml_switchboard"
  createdb -h "$sock_dir" -p "$pg_port" -U "$user" tml_switchboard
fi

# --- NATS + JetStream: bootstrap auth once, render config every run ----
# Log streaming ships supervisor console output to a per-job JetStream
# stream; the switchboard mints per-job bearer JWTs signed with an
# account seed. On first run we generate a throwaway operator/account
# hierarchy (decentralized JWT auth, MEMORY resolver) under the state
# dir. The PUBLIC operator/account JWTs go into the server config; the
# SECRET account seed is handed to the switchboard via the environment,
# never written to its on-disk config (project convention).
nats_dir="$state_dir/nats"
nats_store="$nats_dir/jetstream"
# Scope nsc's stores and keyring to the state dir so it never touches
# the developer's real ~/.config nsc keychain.
export XDG_CONFIG_HOME="$nats_dir/nsc-config"
export XDG_DATA_HOME="$nats_dir/nsc-data"
export NKEYS_PATH="$nats_dir/keys"
mkdir -p "$nats_store" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$NKEYS_PATH"

if [ ! -f "$nats_dir/bootstrapped" ]; then
  echo "Bootstrapping NATS auth (operator tml-dev / account TMLLOGS)"
  nsc add operator --name tml-dev --sys >/dev/null
  nsc add account --name TMLLOGS >/dev/null
  nsc edit account --name TMLLOGS \
    --js-streams -1 \
    --js-consumer -1 \
    --js-mem-storage -1 \
    --js-disk-storage -1 \
    --js-max-mem-stream -1 \
    --js-max-disk-stream -1 \
    >/dev/null
  touch "$nats_dir/bootstrapped"
fi

# The account identity seed signs per-job user JWTs. Export it fresh
# each run (operator key is O-prefixed, the account key A-prefixed) and
# hand it to the switchboard below via the environment.
exp_dir="$nats_dir/exported-keys"
rm -rf "$exp_dir"; mkdir -p "$exp_dir"
nsc export keys --account TMLLOGS --dir "$exp_dir" >/dev/null
nats_account_seed="$(cat "$exp_dir"/A*.nk)"

# Render the server config: the operator JWT + MEMORY resolver preload
# (from nsc), plus a client listener, the JetStream file store, and a
# dev WebSocket listener (ws://, any origin — dev only; production
# origin-allowlists and uses wss://).
nats_conf="$cfg_dir/nats.conf"
nsc generate config --mem-resolver > "$nats_conf"
cat >> "$nats_conf" <<NATSCONF

host: "127.0.0.1"
port: $nats_port

jetstream {
  store_dir: "$nats_store"
}

websocket {
  host: "127.0.0.1"
  port: $nats_ws_port
  no_tls: true
}
NATSCONF

# --- Generate component configs (regenerated every run) ---------------
sb_cfg="$cfg_dir/switchboard.toml"
{
  cat <<TOML
[database]
host = "$sock_dir"
port = $pg_port
database = "tml_switchboard"
user = "$user"
auth.password = ""

[server]
bind_address = "127.0.0.1:$sb_port"

[service]
default_token_timeout = "7d"
default_job_timeout = "30m"
default_queue_timeout = "30m"
match_interval = "10s"
host_liveness_timeout = "30s"
supervisor_ping_interval = "2s"
supervisor_pong_dead = "10s"
supervisor_reconcile_interval = "5s"

[log]
use_tokio_console_subscriber = false

# Log streaming via NATS/JetStream. nats_url is non-secret and lives on
# disk; the account signing seed is injected via the environment
# (TML_LOGSTREAMING__ACCOUNT_SEED) at launch below, not written here.
[log_streaming]
nats_url = "nats://127.0.0.1:$nats_port"

# The console declares its landing URL as each login's return_to; the
# allowlist (exact match) authorizes it. The callback 302s the browser
# there with a single-use staged pair, which the console exchanges
# server-to-server for the session token.
[oauth]
return_to_allowlist = ["http://localhost:$console_port/auth/landing"]

# The mock provider is a development-only, UNAUTHENTICATED login bypass
# (built-in identities, no external service). Safe to enable here only
# because this stack is strictly for local development.
[oauth.mock]
enabled = true
TOML
  if [ "$oauth_enabled" = 1 ]; then
    # Only the non-secret redirect URL is written here; client_id /
    # client_secret arrive via the environment at launch (below).
    cat <<TOML

[oauth.github]
redirect_url = "http://localhost:$sb_port/api/v1/auth/github/callback"
TOML
  fi
} > "$sb_cfg"

console_cfg="$cfg_dir/console.toml"
cat > "$console_cfg" <<TOML
[server]
bind_address = "127.0.0.1:$console_port"
public_base_url = "http://localhost:$console_port"

[switchboard]
base_url = "http://localhost:$sb_port"
TOML

# --- Dev DB seed (applied once at run time, after migrations) ---------
# One admin user 'alice', linked to the built-in mock 'alice' identity
# (and its verified email) so a mock login resolves to it with full
# permissions immediately; it owns the host the supervisor connects as
# plus an API token used by the smoke stage. The host auth_token matches
# the supervisor ws token below. `on conflict do nothing` keeps re-apply
# safe.
seed_sql="$cfg_dir/dev-seed.sql"
cat > "$seed_sql" <<SQL
insert into tml_switchboard.subjects (subject_id, kind)
  values ('$dev_user_id', 'user') on conflict do nothing;
insert into tml_switchboard.users (subject_id, username, full_name, locked)
  values ('$dev_user_id', 'alice', 'Alice Example', false) on conflict do nothing;
insert into tml_switchboard.user_identities (provider, provider_user_id, user_id, provider_login)
  values ('mock', 'alice', '$dev_user_id', 'alice') on conflict do nothing;
insert into tml_switchboard.user_emails (email, user_id, provider, verified)
  values ('alice@example.test', '$dev_user_id', 'mock', true) on conflict do nothing;
insert into tml_switchboard.group_members (group_id, member_id, source, source_ref)
  values ('$admins_group_id', '$dev_user_id', 'manual', '') on conflict do nothing;
insert into tml_switchboard.hosts
  (host_id, name, auth_token, tags, owner_id, ssh_endpoints, current_job)
  values (
    '$host_id', 'dev-qemu-supervisor',
    '\x38292b85b0cc8941bbad8d4b9527f2c01be092a6f50b23add1d8c8828b3d403c',
    '{"host:$host_id"}', '$dev_user_id',
    '{"(127.0.0.1,22)","([::1],22)"}', null
  ) on conflict do nothing;
insert into tml_switchboard.api_tokens
  (token_id, token, user_id, revoked, created_at, expires_at)
  values (
    '3be73eea-192f-46c0-af01-92f574290c81',
    '\x075a32da4a35c1574629bbca73ff5d2a2ee081960b4cbcdd9b602ce02595d797',
    '$dev_user_id', null,
    '2024-07-12 13:56:50.616829-07', '2124-07-12 13:56:50.616829-07'
  ) on conflict do nothing;
SQL

# --- zot registry + qemu supervisor configs (Linux only) -------------
if [ "$enable_supervisor" = 1 ]; then
  cat > "$cfg_dir/zot.json" <<JSON
{"storage":{"rootDirectory":"$zot_dir","dedupe":true},
 "http":{"address":"127.0.0.1","port":"$zot_port"},
 "log":{"level":"error"}}
JSON

  # Per-job writable AAVMF variable store (the code blob stays shared
  # read-only). Modeled on supervisor/qemu/nix_ovmf-vars_start_script.sh.
  aavmf_start="$cfg_dir/aavmf-vars-start.sh"
  cat > "$aavmf_start" <<SH
#!/usr/bin/env bash
set -euo pipefail
cp "$aavmf_vars" "\$TML_JOB_WORKDIR/AAVMF_VARS.fd"
chmod u+w "\$TML_JOB_WORKDIR/AAVMF_VARS.fd"
SH
  chmod +x "$aavmf_start"

  cat > "$cfg_dir/supervisor.toml" <<TOML
[base]
supervisor_id = "$host_id"
coord_connector = "ws_connector"

[ws_connector]
token = "$host_token_bearer"
switchboard_uri = "ws://127.0.0.1:$sb_port"

# Single dev zot: pull from it, and read its blob files off disk
# directly (store_root == the daemon's rootDirectory).
[oci_store]
registry = "127.0.0.1:$zot_port"
store_root = "$zot_dir"

[qemu]
qemu_binary = "qemu-system-aarch64"
qemu_img_binary = "qemu-img"
state_dir = "$sup_state_dir"
# The tiny-efi fixture's layers are 16 MiB; the overlay matches.
working_disk_max_bytes = 16777216
tcp_control_socket_listen_addr = "127.0.0.1:3859"
start_script = "$aavmf_start"
# aarch64 under TCG (no KVM), AAVMF pflash, virtio-blk on the backing
# chain's writable top node. The serial console is wired automatically
# by the supervisor when the dispatch enables log streaming (it ships
# the console to NATS), so it is intentionally not listed here.
qemu_args = [
  "-name", "tml-{job_id}",
  "-nodefaults",
  "-machine", "virt",
  "-cpu", "cortex-a57",
  "-m", "512",
  "-drive", "if=pflash,format=raw,readonly=on,file=$aavmf_code",
  "-drive", "if=pflash,format=raw,file={job_workdir}/AAVMF_VARS.fd",
  "-device", "virtio-blk-device,drive={disk_node}",
  "-netdev", "user,id=net0,hostfwd=tcp::2222-:22",
  "-device", "virtio-net-device,netdev=net0",
  "-fw_cfg", "name=opt/org.tockos.treadmill.tcp-ctrl-socket,string=10.0.2.2:3859",
  "-display", "none",
  "-no-reboot",
]
TOML
fi

# --- Run switchboard + console; tear everything down on exit ----------
pids=()
cleanup() {
  trap - EXIT INT TERM
  echo
  echo "Shutting down dev stack..."
  if [ ${#pids[@]} -gt 0 ]; then
    kill "${pids[@]}" 2>/dev/null || true
    wait 2>/dev/null || true
  fi
  # Only stop Postgres if this invocation started it, so a stack left
  # running in another terminal keeps its database up.
  if [ "$started_pg" = 1 ]; then
    pg_ctl -D "$pg_dir" stop -m fast >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

# Bring up the NATS broker first so the switchboard can create per-job
# streams as jobs dispatch.
echo "Starting NATS (client nats://127.0.0.1:$nats_port, ws ws://127.0.0.1:$nats_ws_port)"
nats-server -c "$nats_conf" -l "$log_dir/nats.log" &
pids+=("$!")

# The single dev zot registry: the supervisor pulls images from it and
# reads its blob files off disk directly. Seeded once with the tiny-efi
# fixture (Linux only).
if [ "$enable_supervisor" = 1 ]; then
  echo "Starting zot registry (http://127.0.0.1:$zot_port)"
  zot serve "$cfg_dir/zot.json" >"$log_dir/zot.log" 2>&1 &
  pids+=("$!")
  if ! wait_http "http://127.0.0.1:$zot_port/v2/" 60; then
    echo "zot did not become ready; see $log_dir/zot.log" >&2
    exit 1
  fi
  if [ ! -e "$zot_dir/.tml-pushed" ]; then
    echo "Pushing tiny-efi fixture into zot (treadmill/tiny-efi:latest)"
    skopeo \
      --registries-conf /dev/null \
      --insecure-policy copy \
      --dest-tls-verify=false \
      "oci:$fixture_layout" \
      "docker://127.0.0.1:$zot_port/treadmill/tiny-efi:latest"
    touch "$zot_dir/.tml-pushed"
  fi
fi

# swx runs sqlx migrations on startup, so the schema is applied here.
# Secrets are injected via the environment, never the config file:
# figment maps TML_OAUTH__GITHUB__* onto oauth.github.* and
# TML_LOGSTREAMING__ACCOUNT_SEED onto log_streaming.account_seed.
if [ "$oauth_enabled" = 1 ]; then
  TML_OAUTH__GITHUB__CLIENT_ID="$TML_DEV_GITHUB_CLIENT_ID" \
  TML_OAUTH__GITHUB__CLIENT_SECRET="$TML_DEV_GITHUB_CLIENT_SECRET" \
  TML_LOG_STREAMING__ACCOUNT_SEED="$nats_account_seed" \
    swx serve -c "$sb_cfg" &
else
  TML_LOG_STREAMING__ACCOUNT_SEED="$nats_account_seed" \
    swx serve -c "$sb_cfg" &
fi
pids+=("$!")

# Wait for the switchboard API (also means migrations have run), then
# seed the dev identities once and register the fixture image.
if ! wait_http "http://127.0.0.1:$sb_port/api/v1/auth/providers" 120; then
  echo "switchboard did not become ready; see $log_dir" >&2
  exit 1
fi

if [ ! -e "$state_dir/db-seeded" ]; then
  echo "Seeding dev identities (admin 'alice', host, API token)"
  psql -h "$sock_dir" -p "$pg_port" -U "$user" -d tml_switchboard \
    -v ON_ERROR_STOP=1 -q -f "$seed_sql"
  touch "$state_dir/db-seeded"
fi

if [ "$enable_supervisor" = 1 ]; then
  manifest_digest="$(jq -r '.manifests[0].digest' "$fixture_layout/index.json")"
  echo "Registering tiny-efi image ($manifest_digest)"
  if RESP="$(\
    curl -fsS -X POST "http://127.0.0.1:$sb_port/api/v1/images" \
     -H "Authorization: Bearer $api_token_bearer" \
     -H 'content-type: application/json' \
     -d "{\"registry\":\"127.0.0.1:$zot_port\",\"repository\":\"treadmill/tiny-efi\",\"manifest_digest\":\"$manifest_digest\",\"label\":\"tiny-efi (dev)\"}" \
  )"; then
    tiny_efi_image_id="$(echo "$RESP" | jq -r .id)"
    echo "  registered as $tiny_efi_image_id"

    # Wrap the fixture image in a public image group so jobs can target a
    # stable moving-target handle (`tiny-efi`) rather than a concrete
    # digest. The group is public, so any mock identity holds `use` on it
    # without an explicit grant; its single generation has one member with
    # no required host tags (selectable on every host).
    echo "Creating tiny-efi image group"
    if GRP="$(\
      curl -fsS -X POST "http://127.0.0.1:$sb_port/api/v1/image-groups" \
       -H "Authorization: Bearer $api_token_bearer" \
       -H 'content-type: application/json' \
       -d "{\"name\":\"tiny-efi\",\"label\":\"tiny-efi (dev)\",\"public\":true}" \
    )"; then
      tiny_efi_group_id="$(echo "$GRP" | jq -r .id)"
      echo "  created as $tiny_efi_group_id"
      if curl -fsS -o /dev/null \
        -X POST "http://127.0.0.1:$sb_port/api/v1/image-groups/$tiny_efi_group_id/generations" \
        -H "Authorization: Bearer $api_token_bearer" \
        -H 'content-type: application/json' \
        -d "{\"members\":[{\"image_id\":\"$tiny_efi_image_id\",\"required_host_tags\":[]}]}"; then
        echo "  added generation with member $tiny_efi_image_id"
      else
        echo "  ! generation creation failed (continuing)" >&2
      fi
    else
      echo "  ! image group creation failed (continuing)" >&2
    fi
  else
    echo "  ! image registration failed (continuing)" >&2
  fi

  echo "Starting QEMU supervisor (aarch64 TCG; host $host_id)"
  treadmill-qemu-supervisor -c "$cfg_dir/supervisor.toml" \
    >"$log_dir/supervisor.log" 2>&1 &
  pids+=("$!")
fi

tml-console serve -c "$console_cfg" &
pids+=("$!")

cat <<EOF

============================================================
  treadmill dev stack is up
    web console     : http://localhost:$console_port   <- open this
    switchboard API : http://localhost:$sb_port
    NATS broker     : nats://127.0.0.1:$nats_port (ws on $nats_ws_port)
    postgresql      : UNIX socket at $sock_dir
      (connect with nix develop -c psql -h $sock_dir -U $user -d tml_switchboard)
    state directory : $state_dir
    mock login      : ENABLED (alice=admin, bob/carol=user)
EOF
if [ "$enable_supervisor" = 1 ]; then
  cat <<EOF
    zot registry    : http://127.0.0.1:$zot_port (repo treadmill/tiny-efi)
    qemu supervisor : host $host_id (aarch64 TCG)
    image group     : tiny-efi (public; targets the tiny-efi fixture)
EOF
else
  echo "    qemu supervisor : DISABLED (non-Linux: tiny-efi fixture unavailable)"
fi
if [ "$oauth_enabled" != 1 ]; then
  cat <<EOF
    GitHub login    : DISABLED
      set TML_DEV_GITHUB_CLIENT_ID and TML_DEV_GITHUB_CLIENT_SECRET,
      with the OAuth app callback set to
      http://localhost:$sb_port/api/v1/auth/github/callback
EOF
else
  echo "    GitHub login    : enabled"
fi
cat <<EOF
  (Ctrl-C to stop. Reset everything with: rm -rf "$state_dir")
============================================================
EOF

if [[ $(type -t TML_DEVSTACK_RUNFN) == function ]]; then
  echo "Running test function TML_DEVSTACK_RUNFN"
  TML_DEVSTACK_RUNFN
else
  # Wait for the components; if any exits, tear the stack down.
  wait
fi


