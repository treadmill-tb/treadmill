# GitHub CI Connector Service

A GitHub App that manages Treadmill CI jobs in response to GitHub repo
events.  The service is built to run as a Fastly Compute program using
the Config and Secret stores.

## Running Locally

You'll need:

- A GitHub App
  - Client ID
  - Private key (generated from the GitHub App settings page)
- The Fastly cli and wasm-tools, available from this directories nix
  flake.
- `rustup` or a Rust toolchain that can build for `wasm32-wasip1`

Modify `fastly.toml`:

- Change `[[local_server.secret_stores.github]] -> file` to the path
  to your private key.

- Change `[[local_server.config_stores.github.contents]] -> clientid`
  to your GitHub App's client id.

You should then be able to run:

```bash
$ fastly compute serve
✓ Verifying fastly.toml
✓ Identifying package name
✓ Identifying toolchain
✓ Running [scripts.build]
✓ Creating package archive

SUCCESS: Built package (pkg/github-treadmill.tar.gz)

✓ Running local server

INFO: Command output:
--------------------------------------------------------------------------------
2026-07-05T03:38:58.461821Z  INFO checking if dictionary adheres to Fastly's API
2026-07-05T03:38:58.461862Z  INFO checking if backend 'githubapi' is up
2026-07-05T03:38:58.501614Z  INFO backend 'githubapi' is up
2026-07-05T03:38:58.501707Z  INFO Listening on http://127.0.0.1:7676
```

However, if this is your first time running `fastly compute serve`
ever, you might see this misleading error instead:

```bash
$ fastly compute serve
IMPORTANT: The Fastly CLI is configured to collect data related to Wasm builds (e.g. compilation times,
resource usage, and other non-identifying data). To learn more about what data is being collected, why, and how to
disable it: https://www.fastly.com/documentation/reference/cli

✓ Verifying fastly.toml
✓ Identifying package name
✓ Identifying toolchain

ERROR: the default build in .fastly/config.toml should produce a wasm32-wasip1 binary, but was instead set to produce a wasm32-wasi binary.

If you believe this error is the result of a bug, please file an issue: https://github.com/fastly/cli/issues/new?labels=bug&template=bug_report.md
```

Indeed your default build is set to `wasm32-wasi`, but the
configuration is possibly somewhere other than
`.fastly/config.toml`. For me it is in `~/.config/fastly/config.toml`.

Wherever your config is, you should change `[language.rust]` block to
include the right `wasm_wasi_target`:

```toml
[language.rust]
wasm_wasi_target = "wasm32-wasip1"
```

Running `fastly compute serve` again and forever should work.
