# TODOS

Deferred work and known shortcuts, captured so they aren't lost. Not a roadmap —
just things we consciously punted on.

## Console / OAuth browser login: harden the token handoff

The `treadmill-console` web frontend logs a browser in by sending it through
switchboard's existing GitHub OAuth flow. Switchboard's callback was built to
return the session token as a JSON body (for programmatic/CLI clients). To make
a no-JS, server-rendered browser login work, the callback gained an optional
`oauth.github.browser_success_redirect` config: when set, it `302`-redirects the
browser to the console with `?token=…&expires_at=…` instead of returning JSON.
The console stores the token in its session cookie and immediately redirects to
strip it from the URL.

**Shortcut:** the session token transits once through a redirect **URL query
string**, so it can land in browser history and (absent a strict referrer
policy) a `Referer` header. Acceptable for the current pre-production prototype;
not acceptable long-term.

**Hardening (do before the console is exposed):** replace the token-in-URL
redirect with a **back-channel one-time-code exchange**:

- The console initiates login with a per-request `return_to`, validated by
  switchboard against a configured **allowlist** (so any console host, not a
  single hard-coded URL, can be supported safely).
- The callback `302`s to `return_to` with a **short-lived, single-use code**
  (new table or reuse the `oauth_flows` mechanism), not the token.
- The console exchanges that code **server-to-server** at a new switchboard
  endpoint (e.g. `POST /auth/exchange`) for the real token.

Net effect: the bearer token never touches the browser or any URL. This also
generalizes the single `browser_success_redirect` value into a proper
multi-origin `return_to` allowlist.

## Re-Sync Verified/Non-Verified Emails on Login

Currently, the auth code does not properly re-sync emails on login. Previously
unverified emails that are now verified upstream aren't marked as such, emails
aren't removed, and now-unverified emails aren't transitioned in the switchboard
DB.

## Separate `manage` Permission from `own` Permission

Currently, `manage` is the same as ownership: it allows to change ownership
arbitrary. There should be a more granular permission that allows to change
attributes of the resource (such as a host's label or tag set), without being
able to change the ACLs or ownership attribute.
