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

  cargoArtifacts = craneLib.buildDepsOnly cargoCommonArgs;

  individualCrateArgs = cargoCommonArgs // {
    inherit cargoArtifacts;
    doCheck = false;
  };

  mkBin =
    {
      crate,
      bin,
      extraBuildInputs ? [ ],
      extraEnv ? { },
    }:
    craneLib.buildPackage (
      individualCrateArgs
      // {
        pname = bin;
        cargoExtraArgs = "--locked -p ${crate} --bin ${bin}";
        buildInputs = individualCrateArgs.buildInputs ++ extraBuildInputs;
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
    cargoArtifacts
    individualCrateArgs
    mkBin
    ;
}
