# treadmill-cli

`tml`, the command-line client for the
[Treadmill](https://treadmill.dev) distributed hardware testbed — and the
agent that runs inside Treadmill job images.

```console
$ tml login
$ tml job create
$ tml job wait
$ tml job ssh
```

## The two halves

One crate, two additive features:

- **`user`** — the client on a user's machine: `login`, `job`, `host`,
  `context` and the `ssh` family, including `tml job ws-proxy`, which bridges
  a connection to a job service through the gateway.
- **`daemon`** — `tml daemon`, the agent inside an image. It takes the job's
  own switchboard token from the supervisor, writes the job environment under
  `/run/tml`, announces the services the image declares in
  `/etc/tml/services.d`, and publishes the credentials on D-Bus so that the
  rest of `tml` works inside a job with no user logged in.

A plain `cargo build` enables both. The published `tml` package builds only
`user`; the static `tml-static-*` packages, for musl, only `daemon`.

## Configuration

A profile-nested TOML file at `$XDG_CONFIG_HOME/treadmill/config.toml`,
layered under `TML_*` environment variables. `--profile` selects one;
`tml` writes a default on first run.

```toml
[default]
switchboard = "https://swb.treadmill.dev"
ssh_domains = ["job.treadmill.dev", "user-gw.treadmill.dev"]
```

The session token lives separately, at
`$XDG_STATE_HOME/treadmill/<profile>.json`, mode `0600`.

## Output

`-o json` prints a command's result as one object; `-o jsonl` streams every
event as a line. Both are covered by snapshot tests, so they are safe to parse.

## Building on it

The mechanics behind these commands — profiles, the token store, the login
flow, service credentials, the WebSocket bridge — are a library of their own,
[`treadmill-client`](https://crates.io/crates/treadmill-client), so another
tool can do what `tml` does without shelling out to it.

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or
[MIT license](../LICENSE-MIT) at your option.
