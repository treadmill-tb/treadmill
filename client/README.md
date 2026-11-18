# treadmill-client

Client-side mechanics for the [Treadmill](https://treadmill.dev) distributed
hardware testbed.

[`treadmill-rs`](https://crates.io/crates/treadmill-rs) defines the wire types
and, behind its `client` feature, a typed HTTP client for the switchboard API.
This crate is the layer above it — the part a client needs that is not a
request or a response:

| | |
|---|---|
| `config`, `state` | the profile on disk, and the session token beside it |
| `session` | one profile resolved: which switchboard, as whom |
| `login` | driving a staged login to completion |
| `service` | minting and caching a job's service credentials |
| `wsproxy` | the WebSocket bridge to a job service, through the gateway |
| `ssh` | what an SSH client has to be told to reach a job |
| `daemon` | asking the in-job `tml daemon` for the job's own credentials |
| `job`, `locked_file`, `error`, `diagnostics` | the pieces those are built from |

`tml` (the `treadmill-cli` crate) is built on this, and is the worked
reference for how the pieces fit together.

## Using it

```rust,no_run
use std::sync::Arc;
use treadmill_client::diagnostics::Silent;
use treadmill_client::session::{Options, Session};

# async fn example() -> anyhow::Result<()> {
// The same profile `tml` would use, and the session `tml login` stored.
let session = Session::load(&Options::default(), Arc::new(Silent))?;
let client = session.authenticated_client()?;
println!("{}", client.whoami().await?.name);
# Ok(())
# }
```

`client/examples/run-job.rs` goes the whole way: enqueue a job, wait for it,
and open a local port onto its SSH service.

## Diagnostics

Nothing here writes to a terminal. What a human might want to see — a cache
file that had to be discarded, the URL a socket was opened against — is
reported through a `Diagnostics` the caller supplies, and `Silent` discards it.

## Features

- `profile` *(default)* — the config file and everything that needs to know
  which switchboard to talk to. Pulls `figment` and `xdg`.
- `ws` *(default)* — the WebSocket bridge. Pulls `tokio-tungstenite` and
  `rustls`.
- `dbus` — the in-job credential lookup. Off by default: it is only meaningful
  inside a job, and `zbus` is a lot to impose on a consumer that just wants to
  enqueue one.

With `default-features = false` you still get `locked_file`, `state`, `job`,
`ssh`'s grammar and option set, `error` and `diagnostics`, which need nothing
beyond `treadmill-rs`.

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or
[MIT license](../LICENSE-MIT) at your option.
