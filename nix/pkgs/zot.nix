# Zot, an OCI-native registry
#
# Not in nixpkgs, adapted from
# https://raw.githubusercontent.com/ijohanne/nur-packages/refs/heads/master/pkgs/zot/default.nix

{
  lib,
  buildGoModule,
  fetchFromGitHub,
  fetchurl,
  go_1_27,
}:
let
  version = "2.1.23";

  zui = fetchurl {
    url = "https://github.com/project-zot/zui/releases/download/commit-ce3819c/zui.tgz";
    hash = "sha256-g2wQoNCPq4h8nFBeyz9IJsr2Z5waWU8Ep5oO9PoVuAo=";
  };
in
(buildGoModule.override { go = go_1_27; }) {
  pname = "zot";
  inherit version;

  src = fetchFromGitHub {
    owner = "project-zot";
    repo = "zot";
    tag = "v${version}";
    hash = "sha256-Q21qFoZzOpJe7oJP4RSGiBvhPF9znCF2ndegsVSHQnY=";
  };

  vendorHash = "sha256-ILHn4EUCHLpwdgSj/CBlX58R1D2zJhr+H6HkcAOdhis=";
  doCheck = false;

  env.CGO_ENABLED = "0";

  preBuild = ''
    tar xzf ${zui} -C pkg/extensions/
  '';

  tags = [
    "sync"
    "search"
    "scrub"
    "metrics"
    "lint"
    "ui"
    "mgmt"
    "profile"
    "userprefs"
    "imagetrust"
    "events"
  ];

  ldflags = [
    "-s"
    "-w"
    "-X zotregistry.dev/zot/v2/pkg/buildinfo.ReleaseTag=${version}"
    "-X zotregistry.dev/zot/v2/pkg/buildinfo.Commit=v${version}"
    "-X zotregistry.dev/zot/v2/pkg/buildinfo.BinaryType=zot-full"
  ];

  subPackages = [
    "cmd/zot"
    "cmd/zli"
  ];

  meta = {
    description = "OCI-native container registry";
    homepage = "https://zotregistry.dev";
    license = lib.licenses.asl20;
    mainProgram = "zot";
  };
}
