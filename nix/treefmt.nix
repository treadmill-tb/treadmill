_: {
  perSystem = _: {
    treefmt = {
      projectRootFile = "flake.nix";

      programs = {
        rustfmt.enable = true;
        nixfmt.enable = true;
        statix.enable = true;
        deadnix.enable = true;
        taplo.enable = true;
      };

      settings.global.excludes = [
        "target/**"
        ".sqlx/**"
        "Cargo.lock"
        "flake.lock"
        "*.lock"
        "*.json"
        "*.sql"
        "*.md"
        "switchboard/flyio/**"
        "switchboard/sql-formatter.json"
      ];
    };
  };
}
