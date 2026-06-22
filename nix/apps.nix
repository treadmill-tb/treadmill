_: {
  perSystem =
    {
      pkgs,
      self',
      ...
    }:
    let
      inherit (pkgs) lib;
      inherit (pkgs.stdenv) isLinux;

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
      devstack = pkgs.writeShellApplication {
        name = "treadmill-devstack";
        runtimeInputs = [
          pkgs.postgresql
          # NATS broker + nsc to bootstrap its decentralized-JWT auth hierarchy
          # for log streaming.
          pkgs.nats-server
          pkgs.nsc
          # Used by the readiness polls, the dev seed/registration, and the smoke
          # stage on every platform.
          pkgs.curl
          pkgs.jq
          self'.packages.swx
          self'.packages.tml-console
        ]
        # The single dev zot + qemu-supervisor stack is Linux-only: it boots the
        # aarch64 tiny-efi fixture, which is only built on Linux (nix/tiny-efi.nix).
        ++ lib.optionals isLinux [
          self'.packages.zot
          self'.packages.treadmill-qemu-supervisor
          pkgs.skopeo
          pkgs.qemu
        ];
        text = ''
          # Inject Nix-specific variables:
          export TML_FIXTURE_LAYOUT="${lib.optionalString isLinux "${self'.packages.tiny-efi-image-layout}"}"
          export TML_AAVMF_CODE="${lib.optionalString isLinux "${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd"}"
          export TML_AAVMF_VARS="${lib.optionalString isLinux "${pkgs.qemu}/share/qemu/edk2-arm-vars.fd"}"

          # This function references variables that will be defined by `devstack.sh`:
          # shellcheck disable=SC2154
          function TML_DEVSTACK_RUNFN() {
            # --- Smoke stage: submit a tiny-efi job and watch it dispatch --------
            # Exercises the real control plane end to end (enqueue -> schedule ->
            # dispatch over the host WS -> image pull -> qemu boot). Polls only
            # until the job reaches Booting (or otherwise advances past queued); the
            # guest then prints its serial sentinel and shuts itself down. Echoes
            # each request.
            set +e
            echo
            echo "=== smoke test: submitting a tiny-efi job ==="
            job_body="{\"init_spec\":{\"type\":\"image\",\"image\":\"$tiny_efi_image_id\"},\"ssh_keys\":[],\"restart_policy\":{\"remaining_restart_count\":0},\"parameters\":{},\"host_tag_requirements\":[\"host:$host_id\"],\"override_timeout\":null}"
            echo "+ curl -X POST http://127.0.0.1:$sb_port/api/v1/jobs  (image=$tiny_efi_image_id, host:$host_id)"
            job_id="$(curl -fsS -X POST "http://127.0.0.1:$sb_port/api/v1/jobs" \
              -H "Authorization: Bearer $api_token_bearer" \
              -H 'content-type: application/json' \
              -d "$job_body" | jq -r '.job_id // empty')"
            if [ -z "$job_id" ]; then
              echo "  ! job submission failed; inspect $log_dir and the console." >&2
              exit 0
            fi
            echo "  submitted job_id=$job_id"
            reached=0
            for _ in $(seq 1 40); do
              echo "+ curl http://127.0.0.1:$sb_port/api/v1/jobs/$job_id"
              info="$(curl -fsS "http://127.0.0.1:$sb_port/api/v1/jobs/$job_id" \
                -H "Authorization: Bearer $api_token_bearer")"
              if [ -n "$info" ]; then
                state="$(echo "$info" | jq -r '.state')"
                stage="$(echo "$info" | jq -r '.initializing_stage // "-"')"
                host="$(echo "$info" | jq -r '.dispatched_on_host_id // "-"')"
                echo "  state=$state stage=$stage host=$host"
                case "$state" in
                  initializing)
                    if [ "$stage" = booting ]; then
                      echo "  reached Booting on the qemu supervisor"
                      reached=1
                      break
                    fi
                    ;;
                  ready | terminating | finalized)
                    echo "  job advanced to $state"
                    reached=1
                    break
                    ;;
                esac
              fi
              sleep 2
            done
            if [ "$reached" != 1 ]; then
              echo "  ! job did not advance past queued within the smoke window;" >&2
              echo "    inspect $log_dir/supervisor.log and the console." >&2
            fi
            echo "=== smoke test done ==="
          }
          export -f TML_DEVSTACK_RUNFN

          # Dispatch to the external (shared with non-Nix users) devstack
          # script:
          exec ${../tools/devstack.sh}
        '';
      };

      # `nix run .#qemu-supervisor-local` -- run the QEMU supervisor standalone,
      # without the switchboard/console/DB/NATS control plane. Brings up a
      # per-developer zot that sources an image from an upstream registry
      # (ghcr.io by default), then drives a single job through the
      # switchboard-less `local` connector (connector/local). The guest console
      # streams to the terminal; Ctrl-C stops the job and tears down.
      #
      # Linux-only (it needs the qemu supervisor + zot, which are themselves
      # Linux-only here); on other systems the runtime inputs and firmware are
      # omitted and the script will report the missing firmware.
      qemu-supervisor-local = pkgs.writeShellApplication {
        name = "treadmill-qemu-supervisor-local";
        runtimeInputs = [
          pkgs.coreutils
          pkgs.curl
        ]
        ++ lib.optionals isLinux [
          self'.packages.zot
          self'.packages.treadmill-qemu-supervisor
          pkgs.skopeo
          pkgs.qemu
        ];
        text = ''
          # Supervisor binary + per-arch UEFI firmware blobs, injected from Nix
          # (empty off Linux; the script errors out cleanly then).
          export TML_SUPERVISOR_BIN="${lib.optionalString isLinux "${self'.packages.treadmill-qemu-supervisor}/bin/treadmill-qemu-supervisor"}"
          export TML_OVMF_CODE="${lib.optionalString isLinux "${pkgs.qemu}/share/qemu/edk2-x86_64-code.fd"}"
          export TML_OVMF_VARS="${lib.optionalString isLinux "${pkgs.qemu}/share/qemu/edk2-i386-vars.fd"}"
          export TML_AAVMF_CODE="${lib.optionalString isLinux "${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd"}"
          export TML_AAVMF_VARS="${lib.optionalString isLinux "${pkgs.qemu}/share/qemu/edk2-arm-vars.fd"}"

          exec ${../tools/local-supervisor.sh} "$@"
        '';
      };
    in
    {
      apps.view-openapi = {
        type = "app";
        program = "${view-openapi}/bin/view-openapi";
        meta.description = "Render and browse the switchboard OpenAPI spec (Redoc)";
      };

      apps.devstack = {
        type = "app";
        program = "${devstack}/bin/treadmill-devstack";
        meta.description = "Run the local dev stack (switchboard + web console)";
      };

      apps.qemu-supervisor-local = {
        type = "app";
        program = "${qemu-supervisor-local}/bin/treadmill-qemu-supervisor-local";
        meta.description = "Run the QEMU supervisor standalone against a local zot (no switchboard)";
      };
    };
}
