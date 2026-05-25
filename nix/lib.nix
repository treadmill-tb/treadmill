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

  src = lib.fileset.toSource {
    root = workspaceRoot;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources workspaceRoot)
      (lib.fileset.maybeMissing (workspaceRoot + "/.sqlx"))
      (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/migrations"))
      (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/SCHEMA.sql"))
      (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/FIXTURES.sql"))
      (lib.fileset.maybeMissing (workspaceRoot + "/switchboard/config.example.toml"))
    ];
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
  pFlagsFor = crates: lib.concatMapStringsSep " " (c: "-p ${c}") crates;

  mkGroupDeps =
    name: crates:
    craneLib.buildDepsOnly (
      cargoCommonArgs
      // {
        pname = "treadmill-${name}";
        cargoExtraArgs = "--locked ${pFlagsFor crates}";
        doCheck = false;
        cargoCheckExtraArgs = "";
      }
    );

  mkGroup = name: crates: {
    inherit crates;
    deps = mkGroupDeps name crates;
  };

  groups = {
    cli = mkGroup "cli" [ "treadmill-cli" ];
    switchboard = mkGroup "switchboard" [ "treadmill-switchboard" ];
    puppet = mkGroup "puppet" [ "treadmill-puppet" ];
    supervisors = mkGroup "supervisors" [
      "treadmill-qemu-supervisor"
      "treadmill-nbd-netboot-supervisor"
      "treadmill-mock-supervisor"
    ];
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
