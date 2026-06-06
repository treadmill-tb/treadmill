# Project Zot — the OCI-native registry used as both the canonical tier and the
# per-server single-writer store/pull-through cache of the Treadmill image
# migration (doc/oci-image-migration-plan.md D10/§7). Not in nixpkgs, so we
# vendor it here. Adapted from
# https://raw.githubusercontent.com/ijohanne/nur-packages/refs/heads/master/pkgs/zot/default.nix
# (the upstream `sources.zot` is replaced with a pinned fetchFromGitHub, and the
# Go toolchain is pinned to 1.25 for the `jsonv2` GOEXPERIMENT).
{
  lib,
  buildGoModule,
  fetchFromGitHub,
  fetchurl,
  go_1_25,
}:
let
  zui = fetchurl {
    url = "https://github.com/project-zot/zui/releases/download/commit-111cb8e/zui.tgz";
    hash = "sha256-cuiUi764XHZZlR1JrkCSvnrkx6XvKvyHgFctCWK/a6g=";
  };
in
(buildGoModule.override { go = go_1_25; }) {
  pname = "zot";
  version = "2.1.15";

  src = fetchFromGitHub {
    owner = "project-zot";
    repo = "zot";
    rev = "v2.1.15";
    hash = "sha256-PhPhlifLU0kOGPqH1kqKxikt+DtJO+MWEv1w6/7sZ6E=";
  };

  vendorHash = "sha256-AhzrYlRE1trshVtXKLiOcwSZzdd8SSDuPz9BNKuwRFs=";
  doCheck = false;

  env = {
    CGO_ENABLED = "0";
    GOEXPERIMENT = "jsonv2";
  };

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
    "-X zotregistry.dev/zot/v2/pkg/api/config.ReleaseTag=v2.1.15"
    "-X zotregistry.dev/zot/v2/pkg/api/config.BinaryType=zot-full"
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
