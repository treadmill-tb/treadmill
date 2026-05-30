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

  # Empty `src/lib.rs` stubs for every workspace member NOT in `included`.
  # Cargo requires each member to declare at least one target; for crates
  # outside this bin's path-dep closure we just need *something*. Stubs are
  # bytewise identical and cover only excluded members, so they don't
  # collide with real source files and never invalidate the bin cache.
  mkWorkspaceStubs =
    included:
    let
      excluded = lib.subtractLists included allMembers;
    in
    pkgs.runCommandLocal "treadmill-workspace-stubs" { } ''
      mkdir -p $out
      ${lib.concatMapStringsSep "\n" (m: ''
        mkdir -p $out/${m}/src
        touch $out/${m}/src/lib.rs
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

  pFlagsFor = crates: lib.concatMapStringsSep " " (c: "-p ${c}") crates;

  # Multiple dependency-only derivations for diverging feature graphs.
  #
  # The dependency layer caching is not very effective for diverging feature
  # graphs; cargo will re-built a large portion of the dependency graph for
  # crates that don't share their features with the cumulative feature set of
  # the workspace.
  #
  # To counter this, we make sure that workspace crates with divergent feature
  # graphs (switchboard's console-subscriber + sqlx, cli's reqwest + ssh2,
  # puppet's zbus) get their own deps layer; the three supervisors share one.
  #
  # Each binary build is invoked with the same `-p` selection as its group's
  # deps layer plus `--bin <name>`, so cargo's feature unification matches and
  # the cached dep artifacts are reused verbatim. `doCheck = false +
  # cargoCheckExtraArgs = ""` keep dev-deps and `--all-targets` out of the graph
  # for the same reason, the bin build doesn't activate them either.
  mkGroupDeps =
    name: crates:
    craneLib.buildDepsOnly (
      cargoCommonArgs
      // {
        src = depsSrc;
        pname = "treadmill-${name}";
        cargoExtraArgs = "--locked ${pFlagsFor crates}";
        doCheck = false;
        cargoCheckExtraArgs = "";
      }
    );

  # `bins` maps each binary name to the workspace members (relative paths
  # under workspaceRoot) whose sources that bin's compile touches: its own
  # crate dir + every transitive path-dep. `extra` is for non-Rust inputs
  # (e.g. sqlx data) shared across the group. Each entry yields a
  # `binSrcs.<bin>` source containing workspace skeleton + included member
  # sources + empty `src/lib.rs` stubs for every other member (so cargo's
  # "every workspace member needs a target" check is satisfied without
  # pulling in their real source). Editing a sibling bin in the same group
  # therefore doesn't invalidate this one. The deps layer is still shared
  # per group: it's invoked with `-p` for every group crate, so its feature
  # unification (and the cached dep artifacts) match every bin build in the
  # group.
  mkGroup =
    {
      name,
      crates,
      bins,
      extra ? [ ],
    }:
    {
      inherit crates;
      deps = mkGroupDeps name crates;
      binSrcs = lib.mapAttrs (
        binName: members:
        mkBinSrc {
          name = binName;
          inherit members extra;
        }
      ) bins;
    };

  groups = {
    cli = mkGroup {
      name = "cli";
      crates = [ "treadmill-cli" ];
      bins.tml = [
        "cli"
        "treadmill-rs"
      ];
    };

    switchboard = mkGroup {
      name = "switchboard";
      crates = [ "treadmill-switchboard" ];
      bins.swx = [
        "switchboard"
        "treadmill-rs"
      ];
      extra = [ switchboardData ];
    };

    puppet = mkGroup {
      name = "puppet";
      crates = [ "treadmill-puppet" ];
      bins.tml-puppet = [
        "puppet"
        "treadmill-rs"
        "control-socket/tcp/client"
      ];
    };

    supervisors =
      let
        shared = [
          "treadmill-rs"
          "control-socket/tcp/server"
          "connector/ws"
        ];
        withLib = shared ++ [ "supervisor/lib" ];
      in
      mkGroup {
        name = "supervisors";
        crates = [
          "treadmill-qemu-supervisor"
          "treadmill-nbd-netboot-supervisor"
          "treadmill-mock-supervisor"
        ];
        bins = {
          treadmill-qemu-supervisor = withLib ++ [ "supervisor/qemu" ];
          treadmill-nbd-netboot-supervisor = withLib ++ [ "supervisor/nbd-netboot" ];
          # mock doesn't depend on supervisor/lib.
          treadmill-mock-supervisor = shared ++ [ "supervisor/mock" ];
        };
      };
  };

  # Workspace-wide deps layer for clippy / `cargo build --workspace` checks
  # that legitimately span the whole graph. Not used by per-binary builds.
  workspaceDeps = craneLib.buildDepsOnly (
    cargoCommonArgs
    // {
      pname = "treadmill-workspace";
    }
  );

  mkBin =
    {
      group,
      bin,
      extraBuildInputs ? [ ],
      extraEnv ? { },
    }:
    craneLib.buildPackage (
      cargoCommonArgs
      // {
        src = group.binSrcs.${bin};
        pname = bin;
        cargoArtifacts = group.deps;
        cargoExtraArgs = "--locked ${pFlagsFor group.crates} --bin ${bin}";
        buildInputs = cargoCommonArgs.buildInputs ++ extraBuildInputs;
        doCheck = false;
      }
      // extraEnv
    );
in
{
  inherit
    toolchain
    craneLib
    src
    cargoCommonArgs
    groups
    mkBin
    workspaceDeps
    ;
}
