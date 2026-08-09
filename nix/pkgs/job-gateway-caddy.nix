# Caddy with JWT verification, as the dev stack's job service gateway.
#
# A job's services are published at `<service>-<job-id>.<domain>` and admitted
# only against a switchboard-minted, EdDSA-signed token. Upstream Caddy cannot
# verify a JWT at all, so the gateway needs one plugin: `caddy-jwt` validates
# the token against the switchboard's public key and exposes its claims as
# placeholders, which is what lets the site config compare the token's audience
# to the host it arrived on and proxy to the address the token names.
{ pkgs }:
pkgs.caddy.withPlugins {
  plugins = [ "github.com/ggicci/caddy-jwt@v1.2.0" ];
  hash = "sha256-CwYRhKkrzLfYBq/K5cKyMgjxdKYlPTwOboALqif7+HU=";
}
