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
  rev = "2bfe843171d8846e12ea423bf108e04d12eef5a3";
  # The stable 2.1.21 release doesn't compile under Go 1.27 because the trivy
  # dependency chokes on stablization differences of the jsonv2 experiment.
  # Unstable upstream already pins a trivy revision with this fixed.
  version = "2.1.21-unstable-2026-09-20";

  zui = fetchurl {
    url = "https://github.com/project-zot/zui/releases/download/commit-a7feb46/zui.tgz";
    hash = "sha256-sV0TxPWLekQAbE0aKyI7vcFI9Wo/zo+0gvSyb6EwXF4=";
  };
in
(buildGoModule.override { go = go_1_27; }) {
  pname = "zot";
  inherit version;

  src = fetchFromGitHub {
    owner = "project-zot";
    repo = "zot";
    inherit rev;
    hash = "sha256-1Ik3Bcy1W2DbpVqfw+BT8p7gh+5trAOrjvLDfioAR6o=";
  };

  vendorHash = "sha256-f+u3wsd+yBTgW7HAAphSFi1KUKbmLC0kkLq71HMfMbM=";
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
    "-X zotregistry.dev/zot/v2/pkg/buildinfo.Commit=${rev}"
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
