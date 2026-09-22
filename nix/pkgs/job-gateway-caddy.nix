# Caddy with JWT verification, as the dev stack's job service gateway and as the
# reverse proxy an image runs in front of a job's own services.
#
# A job's services are published at `<service>-<job-id>.<domain>` and admitted
# only against a switchboard-minted, EdDSA-signed token. Upstream Caddy cannot
# verify a JWT at all, so this needs one plugin: `caddy-jwt` validates the token
# against the switchboard's public key and exposes its claims as placeholders,
# which is what lets a site config compare the token's audience to the host it
# arrived on and proxy to the address the token names.
{
  pkgs,
  # A platform (e.g. `lib.systems.elaborate "aarch64-linux"`) to build a
  # static, interpreter-free binary for, or null for a regular native build.
  staticFor ? null,
}:
let
  base = pkgs.caddy.withPlugins {
    plugins = [ "github.com/ggicci/caddy-jwt@v1.2.0" ];
    hash = "sha256-jaCgIe5sAXMIefSQj4qp2+RLT/9S4fge8rb6cqRYgU4=";
  };

  canExecute = pkgs.stdenv.buildPlatform.canExecute staticFor;
in
if staticFor == null then
  base
else
  # With cgo disabled, Go links a static binary for any target on its own, so
  # this uses the native Go and only sets the target. The pkgsCross/pkgsStatic
  # route instead needs a Go toolchain built for that target, which Hydra does
  # not cache: CI compiled one from source per target, ~11 minutes.
  base.overrideAttrs (prev: {
    env = prev.env // {
      inherit (staticFor.go) GOOS GOARCH;
      CGO_ENABLED = "0";
    };

    # A cross-compiling `go install` puts binaries under bin/<GOOS>_<GOARCH>/.
    # buildGoModule only moves them up when the whole stdenv is cross, which
    # this is not.
    postBuild = (prev.postBuild or "") + ''
      dir=$GOPATH/bin/''${GOOS}_''${GOARCH}
      if [[ -d $dir ]]; then
        mv $dir/* $dir/..
        rmdir $dir
      fi
    '';

    # Same sources and tests as the native job-gateway-caddy, which runs them.
    doCheck = false;

    # nixpkgs' postInstall generates the manpages and shell completions by
    # running the binary, as does the build-info install check; neither works
    # when the build machine cannot execute the target. Keep just the systemd
    # units then.
    postInstall =
      if canExecute then
        prev.postInstall
      else
        ''
          install -Dm644 ${prev.passthru.dist}/init/caddy.service ${prev.passthru.dist}/init/caddy-api.service -t $out/lib/systemd/system
          substituteInPlace $out/lib/systemd/system/caddy.service \
            --replace-fail "/usr/bin/caddy" "$out/bin/caddy"
          substituteInPlace $out/lib/systemd/system/caddy-api.service \
            --replace-fail "/usr/bin/caddy" "$out/bin/caddy"
        '';
    doInstallCheck = canExecute;

    # The binary is copied into images for non-NixOS targets, where a leftover
    # ELF interpreter would make it fail to exec at all. Assert it is really
    # interpreter-free rather than trusting the build flags.
    postFixup = (prev.postFixup or "") + ''
      if ${pkgs.patchelf}/bin/patchelf --print-interpreter $out/bin/caddy 2>/dev/null; then
        echo "caddy has an ELF interpreter; it must be static to run inside an image" >&2
        exit 1
      fi
    '';
  })
