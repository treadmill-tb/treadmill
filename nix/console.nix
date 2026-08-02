{ self, ... }:
{
  perSystem =
    { pkgs, ... }:
    let
      mkConsole = pkgs.lib.makeOverridable (
        {
          apiUrl ? "",
          # `self.dirtyShortRev` would change on every uncommitted edit anywhere
          # in the tree, so omitting that information here.
          rev ? (self.shortRev or "unknown"),
        }:
        pkgs.buildNpmPackage {
          pname = "treadmill-console";
          version = "0.1.0";

          src = pkgs.lib.fileset.toSource {
            root = ../.;
            fileset = pkgs.lib.fileset.unions [
              ../console
              ../switchboard/api-spec/openapi.yaml
            ];
          };
          sourceRoot = "source/console";

          npmDeps = pkgs.importNpmLock { npmRoot = ../console; };
          inherit (pkgs.importNpmLock) npmConfigHook;

          nodejs = pkgs.nodejs_22;

          # openapi-typescript's redocly core reads this; keep it from
          # attempting any network in the sandbox.
          REDOCLY_TELEMETRY = "off";

          VITE_TML_API_URL = apiUrl;
          VITE_TML_CONSOLE_REV = rev;

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

            cat > $out/_redirects <<'EOF'
            /*  /index.html  200
            EOF

            cat > $out/_headers <<'EOF'
            /assets/*
              Cache-Control: public, max-age=31536000, immutable
            /index.html
              Cache-Control: no-cache
            EOF

            runHook postInstall
          '';
        }
      );
    in
    {
      # `apiUrl` defaults to same-origin, production uses `console.override {
      # apiUrl = ...; }`
      packages.console = mkConsole { };
    };
}
