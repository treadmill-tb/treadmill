{
  inputs,
  system,
  pkgs,
}:
let
  inherit (pkgs) lib;
  inherit (inputs) crane fenix;

  toolchain = fenix.packages.${system}.stable.toolchain;
  craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

  workspaceRoot = ../.;

  # Workspace skeleton: every Cargo.toml + Cargo.lock. Required for cargo
  # to parse the workspace even when only a subset of members is being
  # built — cargo refuses to load a workspace whose declared members are
  # missing their manifests.
  workspaceSkeleton = craneLib.fileset.cargoTomlAndLock workspaceRoot;

  # Switchboard's non-Rust build inputs: offline sqlx query cache plus
  # files baked in via include_str!/include_bytes! (SCHEMA, FIXTURES,
  # example config) and the migrations directory the binary expects at
  # runtime.
  switchboardData = lib.fileset.unions [
    (lib.fileset.maybeMissing (workspaceRoot + "/.sqlx"))
    (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/migrations"))
    (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/SCHEMA.sql"))
    (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/FIXTURES.sql"))
    (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/config.example.toml"))
    # Committed OpenAPI snapshot read by the api-spec drift-guard test.
    (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/api-spec"))
  ];

  # Union of .rs + .toml under each workspace member in `members`. Used to
  # assemble per-build sources containing only the crate being built plus
  # its workspace-internal path-deps. Members are paths relative to the
  # workspace root, matching the entries in `Cargo.toml [workspace.members]`.
  crateSources =
    members:
    lib.fileset.unions (map (m: craneLib.fileset.commonCargoSources (workspaceRoot + "/${m}")) members);

  # Every entry in `Cargo.toml [workspace.members]`, read directly so this
  # list stays in sync as members are added or removed.
  allMembers =
    (builtins.fromTOML (builtins.readFile (workspaceRoot + "/Cargo.toml"))).workspace.members;

  # Stub sources for every workspace member NOT in `included`. Cargo requires
  # each member to declare at least one target; since bins are built with
  # `--workspace` (so all members are *selected*), a stub must also satisfy any
  # member declaring an explicit `[[bin]] path = "src/main.rs"` (cli, puppet) —
  # hence both stubs. Neither is ever compiled (the build is `--bin <name>`);
  # they only need to exist. Stubs cover only excluded members, so they never
  # collide with real sources or invalidate the bin cache.
  mkWorkspaceStubs =
    included:
    let
      excluded = lib.subtractLists included allMembers;
    in
    pkgs.runCommandLocal "treadmill-workspace-stubs" { } ''
      mkdir -p $out
      ${lib.concatMapStringsSep "\n" (m: ''
        mkdir -p $out/${m}/src
        touch $out/${m}/src/lib.rs $out/${m}/src/main.rs
      '') excluded}
    '';

  # Overlay the per-bin fileset on top of workspace stubs. Stubs first
  # (excluded members only), real fileset second; the real workspace
  # skeleton + included crate sources land cleanly without collisions
  # (stubs are only created where real files won't be).
  mkBinSrc =
    {
      name,
      members,
      extra ? [ ],
    }:
    let
      fsSrc = lib.fileset.toSource {
        root = workspaceRoot;
        fileset = lib.fileset.unions (
          [
            workspaceSkeleton
            (crateSources members)
          ]
          ++ extra
        );
      };
      stubs = mkWorkspaceStubs members;
    in
    pkgs.runCommandLocal "treadmill-${name}-src" { } ''
      mkdir -p $out
      cp -r ${stubs}/. $out/
      chmod -R u+w $out
      cp -rf ${fsSrc}/. $out/
      chmod -R u+w $out
    '';

  # Full workspace src — used by clippy/nextest and the workspace-wide
  # deps layer, which all legitimately span every crate.
  src = lib.fileset.toSource {
    root = workspaceRoot;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources workspaceRoot)
      switchboardData
      # Committed wire-schema snapshots read by the protocol drift-guard test.
      (lib.fileset.maybeMissing (workspaceRoot + "/treadmill-rs/protocol-schema"))
      # Committed `insta` snapshots (`.snap`); not picked up by
      # commonCargoSources, so without this insta tests see no stored snapshot
      # and fail in the sandbox.
      (lib.fileset.fileFilter (file: file.hasExt "snap") workspaceRoot)
    ];
  };

  # Source for *-deps-only builds: skeleton only. crane's mkDummySrc
  # replaces every member's src/{main,lib}.rs with stubs, so the actual
  # crate sources don't affect the dep cache.
  depsSrc = lib.fileset.toSource {
    root = workspaceRoot;
    fileset = workspaceSkeleton;
  };

  cargoCommonArgs = {
    inherit src;
    strictDeps = true;

    SQLX_OFFLINE = "true";

    # sqlx-macros (a proc-macro .so loaded by rustc at compile time) links
    # against libssl.so.3; without this it fails with
    #
    #     libssl.so.3: cannot open shared object file
    #
    # when rustc tries to dlopen the macro.
    LD_LIBRARY_PATH = lib.makeLibraryPath [ pkgs.openssl ];

    nativeBuildInputs = [
      pkgs.pkg-config

      # Rust + openssl-sys leaves binaries dynamically referencing libssl.so.3
      # with no RPATH pointing at it; autoPatchelfHook adds the closure's lib
      # dirs to RUNPATH so `./result/bin/tml` works standalone (not just inside
      # `nix develop`).
      pkgs.autoPatchelfHook
    ];

    buildInputs = [
      pkgs.openssl
      pkgs.libiconv

      # Provides libgcc_s.so.1 in the runtime closure so autoPatchelfHook can
      # patch the binary's RUNPATH to find it.
      pkgs.stdenv.cc.cc.lib
    ];
  };

  # Each bin maps to the workspace members (relative paths under workspaceRoot)
  # whose sources its compile touches: its own crate + every transitive
  # path-dep. `extra` carries non-Rust inputs (e.g. sqlx data) baked into the
  # build. Everything else is stubbed (see mkWorkspaceStubs), so editing a
  # sibling bin never invalidates this one.
  binSources =
    let
      supervisorShared = [
        "treadmill-rs"
        "control-socket/tcp/server"
        "connector/local"
        "connector/ws"
        "supervisor/lib"
      ];
    in
    {
      tml.members = [
        "cli"
        "treadmill-rs"
      ];
      # `swx` can embed the web console, so its compile touches the console
      # crate too (and treadmill-rs, which both share).
      swx = {
        members = [
          "switchboard"
          "treadmill-rs"
          "console"
        ];
        extra = [ switchboardData ];
      };
      tml-console.members = [
        "console"
        "treadmill-rs"
      ];
      # Producer-side OCI layout assemble + validate tool.
      image-util.members = [
        "images/util"
        "treadmill-rs"
      ];
      tml-puppet.members = [
        "puppet"
        "treadmill-rs"
        "control-socket/tcp/client"
      ];
      treadmill-qemu-supervisor.members = supervisorShared ++ [ "supervisor/qemu" ];
      treadmill-nbd-netboot-supervisor.members = supervisorShared ++ [ "supervisor/nbd-netboot" ];
    };

  # Per-bin source derivation (skeleton + the bin's member sources + stubs for
  # every other member). Consumed by mkBin and the cross-musl puppet build.
  binSrcs = lib.mapAttrs (name: args: mkBinSrc ({ inherit name; } // args)) binSources;

  # Single dependency layer shared by every binary. Built with `--workspace` so
  # cargo resolves the full feature union; each bin is then built with
  # `--workspace --bin <name>`, which resolves that same union and reuses these
  # artifacts verbatim — one deps build for all bins, no per-group rebuilds.
  # `doCheck = false` + empty `cargoCheckExtraArgs` keep dev-deps/--all-targets
  # out of the graph, matching what the `--bin` builds actually activate.
  binDeps = craneLib.buildDepsOnly (
    cargoCommonArgs
    // {
      src = depsSrc;
      pname = "treadmill-bin-deps";
      cargoExtraArgs = "--locked --workspace";
      doCheck = false;
      cargoCheckExtraArgs = "";
    }
  );

  # Workspace-wide deps layer for clippy / nextest, which span the whole graph
  # with `--all-targets` (so it includes dev-deps). Distinct from `binDeps`
  # above, whose feature set omits them.
  workspaceDeps = craneLib.buildDepsOnly (
    cargoCommonArgs
    // {
      pname = "treadmill-workspace";
    }
  );

  # Compile-once layer for the test checks: builds every workspace test binary
  # (`--no-run`) on top of workspaceDeps and ships the resulting target/ as a
  # cargoArtifacts layer. The nextest / nextest-db / integration checks consume
  # it and only *run* a filtered subset, so the test binaries compile once
  # instead of once per check. Every consumer selects `--workspace` so cargo's
  # feature unification matches this layer and nothing recompiles.
  testArtifacts = craneLib.mkCargoDerivation (
    cargoCommonArgs
    // {
      pname = "treadmill-tests";
      version = "0.1.0";
      cargoArtifacts = workspaceDeps;
      # `--cargo-profile release` matches what craneLib.cargoNextest injects in
      # the consuming checks; without it the test binaries are built under the
      # `test` profile and every check recompiles them.
      buildPhaseCargoCommand = "cargo nextest run --cargo-profile release --workspace --no-run --no-tests=pass";
      doInstallCargoArtifacts = true;
      nativeBuildInputs = cargoCommonArgs.nativeBuildInputs ++ [ pkgs.cargo-nextest ];
    }
  );

  mkBin =
    {
      bin,
      extraBuildInputs ? [ ],
      extraEnv ? { },
    }:
    craneLib.buildPackage (
      cargoCommonArgs
      // {
        src = binSrcs.${bin};
        pname = bin;
        cargoArtifacts = binDeps;
        cargoExtraArgs = "--locked --workspace --bin ${bin}";
        buildInputs = cargoCommonArgs.buildInputs ++ extraBuildInputs;
        doCheck = false;
      }
      // extraEnv
    );
  # Vendored Project Zot registry (see nix/pkgs/zot.nix) — the per-server store
  # daemon / pull-through cache for the OCI image migration.
  zot = pkgs.callPackage ./pkgs/zot.nix { };

  # Shell snippet to run a throwaway Postgres cluster in a fresh temp
  # dir. Exports DATABASE_URL (+ the PG* / TML_DATABASE__* vars) pointing at it,
  # and forks a PID-keyed watcher that tears the cluster down when the owning
  # process exits.
  ephemeralPostgresHook = ''
    PG_BASE_DIR="$(mktemp -d /tmp/tmlswbmigratedb-XXXXX)"
    export PG_BASE_DIR

    echo "Initializing new Postgres database in $PG_BASE_DIR"
    initdb -D "$PG_BASE_DIR"

    pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" \
      -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

    # Tear down Postgres and its scratch dir once the owning process is gone. A
    # plain `trap ... EXIT` is not enough: `nix develop -c CMD` execs the
    # command, replacing the shell that holds the trap, so it never fires and
    # the detached pg_ctl daemon is orphaned in /tmp. Instead, fork a watcher
    # keyed on this process's PID. exec preserves the PID, so the watcher fires
    # on every exit path -- interactive exit, `-c`, even a killed terminal --
    # and only ever touches its own cluster, so concurrent users don't clean up
    # each other.
    __tml_db_pid=$$
    (
      while kill -0 "$__tml_db_pid" 2>/dev/null; do sleep 1; done
      pg_ctl -D "$PG_BASE_DIR" stop -m immediate >/dev/null 2>&1
      rm -rf "$PG_BASE_DIR"
    ) &
    disown

    export PGHOST="$PG_BASE_DIR"
    PGHOST_URIENCODE="$(printf '%s' "$PGHOST" | ${pkgs.jq}/bin/jq -sRr @uri)"
    export PGHOST_URIENCODE
    PGUSER="$(whoami)"
    export PGUSER
    export PGDATABASE="postgres"
    export DATABASE_URL="postgresql://''${PGUSER}@''${PGHOST_URIENCODE}/''${PGDATABASE}"

    export TML_DATABASE__HOST="$PGHOST"
    export TML_DATABASE__DATABASE="$PGDATABASE"
    export TML_DATABASE__USER="$PGUSER"
  '';
in
{
  inherit
    toolchain
    craneLib
    src
    cargoCommonArgs
    binSrcs
    mkBin
    workspaceDeps
    testArtifacts
    zot
    ephemeralPostgresHook
    ;
}
