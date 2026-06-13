_: {
  perSystem =
    {
      pkgs,
      self',
      ...
    }:
    let
      # The committed switchboard OpenAPI snapshot. Pinned into the store so the
      # app works from a clean checkout; override by passing a path argument
      # (`nix run .#view-openapi -- path/to/spec.yaml`).
      defaultSpec = ../switchboard/api-spec/openapi.yaml;

      # `nix run .#view-openapi` -- render the switchboard OpenAPI spec as a
      # browsable HTML page and open it. Redocly's `build-docs` emits a
      # single self-contained HTML file (the Redoc viewer bundle is inlined),
      # so this needs no network access at run time. We then serve it over a
      # local HTTP server and open the browser, which is the quickest way to
      # eyeball whether the API surface makes sense.
      view-openapi = pkgs.writeShellApplication {
        name = "view-openapi";
        runtimeInputs = [
          pkgs.redocly
          pkgs.python3
          pkgs.xdg-utils
        ];
        text = ''
          set -euo pipefail

          spec="''${1:-${defaultSpec}}"
          port="''${PORT:-8088}"

          workdir="$(mktemp -d)"
          trap 'rm -rf "$workdir"' EXIT

          # Keep the build fully local/offline.
          export REDOCLY_TELEMETRY=off

          echo "Rendering OpenAPI docs from: $spec"
          redocly build-docs "$spec" --output "$workdir/index.html"

          url="http://127.0.0.1:$port/"
          echo "Serving API docs at $url  (Ctrl-C to stop)"

          # Open the browser once the server is up; never fail the app if no
          # browser/opener is available (e.g. headless).
          ( sleep 1; xdg-open "$url" >/dev/null 2>&1 || true ) &

          exec python3 -m http.server "$port" --bind 127.0.0.1 --directory "$workdir"
        '';
      };

      # `nix run .#dev` -- run the whole stack (switchboard + console; supervisors
      # later) locally for development, using the Nix-built binaries.
      #
      # State lives in a persistent directory (default
      # `$XDG_CACHE_HOME/treadmill-devstack`, overridable with TML_DEVSTACK_DIR)
      # so the Postgres cluster and its data survive across runs. An embedded
      # Postgres is initialised on first run and started on a private unix
      # socket; the components are configured to talk to it and to each other.
      #
      # Secrets are NOT baked in: GitHub login is enabled only when both
      # TML_DEV_GITHUB_CLIENT_ID and TML_DEV_GITHUB_CLIENT_SECRET are set in the
      # environment (register a GitHub OAuth app whose callback is
      # `http://localhost:<sb-port>/api/v1/auth/github/callback`). Without them
      # the stack still runs; interactive login is just disabled.
      dev = pkgs.writeShellApplication {
        name = "treadmill-dev";
        runtimeInputs = [
          pkgs.postgresql
          self'.packages.swx
          self'.packages.tml-console
        ];
        text = ''
          set -euo pipefail
          # Config files hold the OAuth client secret; keep them private.
          umask 077

          state_dir="''${TML_DEVSTACK_DIR:-''${XDG_CACHE_HOME:-$HOME/.cache}/treadmill-devstack}"
          sb_port="''${TML_SB_PORT:-8000}"
          console_port="''${TML_CONSOLE_PORT:-8080}"
          pg_port="''${TML_PG_PORT:-5432}"
          user="$(id -un)"

          pg_dir="$state_dir/pg"
          sock_dir="$state_dir/pg-sockets"
          cfg_dir="$state_dir/config"
          log_dir="$state_dir/logs"
          mkdir -p "$state_dir" "$sock_dir" "$cfg_dir" "$log_dir"

          # GitHub login is enabled only when both credentials are present in the
          # environment. The secret is never written to the config file: it is
          # passed to switchboard via TML_OAUTH__GITHUB__* env vars (figment reads
          # TML_-prefixed env, nesting on `__`), leaving only the non-secret
          # redirect URLs on disk.
          if [ -n "''${TML_DEV_GITHUB_CLIENT_ID:-}" ] \
             && [ -n "''${TML_DEV_GITHUB_CLIENT_SECRET:-}" ]; then
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
              -o "-h ''' --unix_socket_directories='$sock_dir' -p $pg_port" start
            started_pg=1
          else
            started_pg=0
          fi

          if ! psql -h "$sock_dir" -p "$pg_port" -U "$user" -d postgres -lqt \
              | cut -d '|' -f1 | grep -qw tml_switchboard; then
            echo "Creating database tml_switchboard"
            createdb -h "$sock_dir" -p "$pg_port" -U "$user" tml_switchboard
          fi

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

          # browser_success_redirect is provider-independent: any OAuth callback
          # 302s the browser here with the freshly minted token, which the
          # console moves into its session cookie.
          [oauth]
          browser_success_redirect = "http://localhost:$console_port/auth/landing"

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

          # --- Run switchboard + console; tear everything down on exit ----------
          pids=()
          cleanup() {
            trap - EXIT INT TERM
            echo
            echo "Shutting down dev stack..."
            if [ ''${#pids[@]} -gt 0 ]; then
              kill "''${pids[@]}" 2>/dev/null || true
              wait 2>/dev/null || true
            fi
            # Only stop Postgres if this invocation started it, so a stack left
            # running in another terminal keeps its database up.
            if [ "$started_pg" = 1 ]; then
              pg_ctl -D "$pg_dir" stop -m fast >/dev/null 2>&1 || true
            fi
          }
          trap cleanup EXIT INT TERM

          # swx runs sqlx migrations on startup, so the schema is applied here.
          # The OAuth secret is injected via the environment, never the config
          # file (figment maps TML_OAUTH__GITHUB__* onto oauth.github.*).
          if [ "$oauth_enabled" = 1 ]; then
            TML_OAUTH__GITHUB__CLIENT_ID="$TML_DEV_GITHUB_CLIENT_ID" \
            TML_OAUTH__GITHUB__CLIENT_SECRET="$TML_DEV_GITHUB_CLIENT_SECRET" \
              swx serve -c "$sb_cfg" &
          else
            swx serve -c "$sb_cfg" &
          fi
          pids+=("$!")
          tml-console serve -c "$console_cfg" &
          pids+=("$!")

          cat <<EOF

          ============================================================
            treadmill dev stack is up
              web console     : http://localhost:$console_port   <- open this
              switchboard API : http://localhost:$sb_port
              state directory : $state_dir
              mock login      : ENABLED (dev only, unauthenticated)
          EOF
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

          # Wait for the components; if either exits, tear the stack down.
          wait
        '';
      };
    in
    {
      apps.view-openapi = {
        type = "app";
        program = "${view-openapi}/bin/view-openapi";
        meta.description = "Render and browse the switchboard OpenAPI spec (Redoc)";
      };

      apps.dev = {
        type = "app";
        program = "${dev}/bin/treadmill-dev";
        meta.description = "Run the local dev stack (switchboard + web console)";
      };
    };
}
