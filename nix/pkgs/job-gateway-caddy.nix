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
  static ? false,
}:
let
  canExecute = pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform;

  base = pkgs.caddy.withPlugins {
    plugins = [ "github.com/ggicci/caddy-jwt@v1.2.0" ];
    hash = "sha256-CwYRhKkrzLfYBq/K5cKyMgjxdKYlPTwOboALqif7+HU=";
    # The check runs `caddy build-info` on the build machine, which a
    # cross-compiled binary cannot do.
    doInstallCheck = canExecute;
  };
in
if !static then
  base
else
  base.overrideAttrs (prev: {
    # The default build links against the Nix store's glibc, ELF interpreter and
    # all. Copied into an image that binary does not fail on a version mismatch,
    # it fails to exec at all -- so the caller must hand this a pkgsStatic set,
    # and this asserts the result really is interpreter-free rather than trusting
    # it to be.
    postFixup = (prev.postFixup or "") + ''
      if ${pkgs.buildPackages.patchelf}/bin/patchelf --print-interpreter $out/bin/caddy 2>/dev/null; then
        echo "caddy has an ELF interpreter; it must be static to run inside an image" >&2
        exit 1
      fi
    '';
  })
