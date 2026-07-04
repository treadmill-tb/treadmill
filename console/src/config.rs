//! Console configuration: where to bind, and which switchboard to talk to.
//!
//! Loaded the same way as the switchboard's own config — an optional TOML file
//! overlaid with `TML_CONSOLE_`-prefixed environment variables (nested keys
//! separated by `__`, e.g. `TML_CONSOLE_SERVER__BIND_ADDRESS`).

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ConsoleConfig {
    /// How the console's own HTTP server is exposed.
    pub server: ServerConfig,
    /// Which switchboard instance to render.
    pub switchboard: SwitchboardConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address the console binds its HTTP listener to.
    pub bind_address: SocketAddr,
    /// The console's own externally reachable origin, e.g.
    /// `https://console.example`. Two uses: the switchboard's
    /// `browser_success_redirect` should point at `<public_base_url>/auth/landing`,
    /// and an `https://` origin makes the session cookie `Secure`.
    pub public_base_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SwitchboardConfig {
    /// Origin of the switchboard API, e.g. `https://switchboard.example`. The
    /// `/api/v1` prefix is added by the client; a trailing slash is ignored.
    pub base_url: String,
}

impl ServerConfig {
    /// Whether session cookies should carry the `Secure` attribute, inferred
    /// from the public origin's scheme.
    pub fn cookies_secure(&self) -> bool {
        self.public_base_url.starts_with("https://")
    }
}

/// Load configuration from an optional TOML file overlaid with environment
/// variables, mirroring the switchboard's loader.
pub fn load_configuration(path: Option<&Path>) -> anyhow::Result<ConsoleConfig> {
    use figment::providers::{self, Format};
    let f = figment::Figment::new();

    let f = if let Some(p) = path {
        if !p.exists() {
            return Err(anyhow::anyhow!(
                "Specified configuration file '{}' does not exist",
                p.display()
            ));
        }
        f.merge(providers::Toml::file(p))
    } else {
        tracing::info!("No configuration file specified");
        f
    };

    f.merge(providers::Env::prefixed("TML_CONSOLE_").split("__"))
        .extract()
        .context("Failed to extract console configuration")
}
