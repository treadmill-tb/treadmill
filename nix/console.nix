_: {
  perSystem =
    { pkgs, ... }:
    {
      # The SPA web console. Building it is also the CI gate for the frontend:
      # the committed OpenAPI-generated types are drift-checked against
      # switchboard/api-spec/openapi.yaml, then lint + typecheck + vite build
      # run in order. Promoted to a flake check by nix/checks.nix.
      packages.console = pkgs.buildNpmPackage {
        pname = "treadmill-console";
        version = "0.1.0";

        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../console
            # The codegen input, addressed as `../switchboard/...` by the
            # `codegen` npm script.
            ../switchboard/api-spec/openapi.yaml
          ];
        };
        sourceRoot = "source/console";

        npmDeps = pkgs.importNpmLock { npmRoot = ../console; };
        inherit (pkgs.importNpmLock) npmConfigHook;

        nodejs = pkgs.nodejs_22;

        # openapi-typescript's redocly core reads this; keep it from attempting
        # any network in the sandbox.
        REDOCLY_TELEMETRY = "off";

        preBuild = ''
          cp app/api/schema.d.ts schema.committed.d.ts
          npm run codegen
          if ! diff -u schema.committed.d.ts app/api/schema.d.ts; then
            echo 'console/app/api/schema.d.ts is out of date with' >&2
            echo 'switchboard/api-spec/openapi.yaml; regenerate it with' >&2
            echo '`npm run codegen` in console/ and commit the diff.' >&2
            exit 1
          fi
          rm schema.committed.d.ts

          npm run lint
          npm run typecheck
        '';

        installPhase = ''
          runHook preInstall
          cp -r build/client $out
          runHook postInstall
        '';
      };
    };
}
