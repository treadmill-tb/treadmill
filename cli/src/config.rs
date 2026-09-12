use anyhow::{Context, Result};
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_SWITCHBOARD: &str = "https://swb.treadmill.dev";
pub const DEFAULT_PROFILE: &str = "default";
const XDG_PREFIX: &str = "treadmill";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_switchboard")]
    pub switchboard: String,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub insecure_tls: bool,
}

fn default_switchboard() -> String {
    DEFAULT_SWITCHBOARD.to_string()
}

fn default_user() -> String {
    "tml".to_string()
}

pub fn config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let dirs = xdg::BaseDirectories::with_prefix(XDG_PREFIX);
    dirs.get_config_file("config.toml")
        .context("no XDG config directory")
}

pub fn state_path(profile: &str) -> Result<PathBuf> {
    let dirs = xdg::BaseDirectories::with_prefix(XDG_PREFIX);
    dirs.place_state_file(format!("{profile}.json"))
        .context("creating the XDG state directory")
}

/// Load the selected profile, layering the config file under `TML_*`
/// environment variables. The environment is `global` so that a variable
/// overrides whichever profile was selected, rather than only the default one.
pub fn load(path: &Path, profile: &str) -> Result<Config> {
    let mut figment = Figment::new();
    if path.exists() {
        figment = figment.merge(Toml::file(path).nested());
    }
    figment
        .select(profile)
        .merge(Env::prefixed("TML_").split("__").global())
        .extract()
        .with_context(|| format!("loading profile {profile:?} from {}", path.display()))
}
