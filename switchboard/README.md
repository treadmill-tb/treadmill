Directory structure:

- `axum-ws-tungstenite` is a patched version of `axum::extract::ws`, à la `axum-tungstenite`, but up to date; instead of
  using `axum`'s generic `WebSocket` type, it directly exposes a `tokio_tungstenite::WebSocketStream`. The primary
  consideration here is actually in error handling, since `axum`'s `WebSocket` uses `axum::Error`, a dynamic error
  type.
- `conf-example` is a set of example config files, for development purposes only.
- `connector` should only be inside `switchboard` temporarily, and is the supervisor side of the supervisor-switchboard
  connection.
- `perm-demo` is a testbed for permissions types. It will be deleted shortly.
- `scripts` are various useful scripts.
- `sql` contains various SQL scripts related to the switchboard database. It should not be used directly.
- `switchboard` is the main `tml-switchboard` crate.