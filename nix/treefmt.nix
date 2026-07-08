_: {
  perSystem =
    {
      pkgs,
      lib,
      config,
      ...
    }:
    {
      treefmt = {
        projectRootFile = "flake.nix";

        programs = {
          rustfmt.enable = true;
          nixfmt.enable = true;
          statix.enable = true;
          deadnix.enable = true;
          taplo.enable = true;
          sql-formatter.enable = true;
          prettier.enable = true;
        };

        settings.global.excludes = [
          "target/**"
          ".sqlx/**"
          "Cargo.lock"
          "flake.lock"
          "*.lock"
          "*.json"
          "*.md"
          "switchboard/flyio/**"
          "switchboard/sql-formatter.json"
          # Generated (openapi-typescript); kept as raw codegen output so the
          # drift check in nix/console.nix is a byte-for-byte diff.
          "console/app/api/schema.d.ts"
          "console/node_modules/**"
          "console/build/**"
          "console/.react-router/**"
        ];

        # Scope prettier to the web console: its default include list would also
        # claim YAMLs like the generated switchboard/api-spec/openapi.yaml, which
        # must stay byte-identical to what the drift test emits.
        settings.formatter.prettier.includes = lib.mkForce [
          "console/**/*.ts"
          "console/**/*.tsx"
          "console/**/*.js"
          "console/**/*.css"
          "console/**/*.html"
        ];

        settings.formatter.sql-formatter =
          let
            cfg = config.treefmt.programs.sql-formatter;
          in
          {
            command = pkgs.lib.mkForce (
              pkgs.writeShellScriptBin "sql-formatter-fix" ''
                for file in "$@"; do
                  ${cfg.package}/bin/sql-formatter \
                    ${pkgs.lib.optionalString (cfg.dialect != null) "-l ${cfg.dialect}"} \
                    --config ${../switchboard/sql-formatter.json} \
                    --fix \
                    $file
                done
              ''
            );
          };
      };
    };
}
