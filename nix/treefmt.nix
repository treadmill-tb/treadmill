_: {
  perSystem = { pkgs, config, ... }: {
    treefmt = {
      projectRootFile = "flake.nix";

      programs = {
        rustfmt.enable = true;
        nixfmt.enable = true;
        statix.enable = true;
        deadnix.enable = true;
        taplo.enable = true;
        sql-formatter.enable = true;
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
