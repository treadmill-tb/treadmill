_: {
  perSystem =
    { pkgs, ... }:
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
    in
    {
      apps.view-openapi = {
        type = "app";
        program = "${view-openapi}/bin/view-openapi";
        meta.description = "Render and browse the switchboard OpenAPI spec (Redoc)";
      };
    };
}
