use anyhow::{Context, Result, bail};
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_SWITCHBOARD: &str = "https://swb.treadmill.dev";
pub const DEFAULT_PROFILE: &str = "default";

const DEFAULT_SSH_DOMAINS: [&str; 2] = ["job.treadmill.dev", "user-gw.treadmill.dev"];
const XDG_PREFIX: &str = "treadmill";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub switchboard: String,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub insecure_tls: bool,
    #[serde(default)]
    pub ssh_domains: Option<Vec<String>>,
}

fn default_user() -> String {
    "tml".to_string()
}

impl Config {
    pub fn ssh_domains(&self, profile: &str) -> Result<&[String]> {
        match self.ssh_domains.as_deref() {
            Some([]) | None => bail!(
                "profile {profile:?} configures no ssh_domains; add to the profile:\n\n    \
                 ssh_domains = {}\n",
                toml_list(&DEFAULT_SSH_DOMAINS)
            ),
            Some(domains) => Ok(domains),
        }
    }
}

fn toml_list(values: &[&str]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| format!("{value:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

fn create(path: &Path, profile: &str) {
    let contents = format!(
        "[{profile}]\nswitchboard = {DEFAULT_SWITCHBOARD:?}\nssh_domains = {}\n",
        toml_list(&DEFAULT_SSH_DOMAINS)
    );
    let written = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(path, contents));
    if let Err(error) = written {
        let style = anstyle::AnsiColor::Yellow.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!(
            "{style}warning:{style:#} creating {}: {error}",
            path.display()
        );
    }
}

pub fn config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let dirs = xdg::BaseDirectories::with_prefix(XDG_PREFIX);
    dirs.get_config_file("config.toml")
        .context("no XDG config directory")
}

pub fn ssh_config_path() -> Result<PathBuf> {
    let dirs = xdg::BaseDirectories::with_prefix(XDG_PREFIX);
    dirs.place_config_file("ssh_config")
        .context("creating the XDG config directory")
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
    if !path.exists() {
        create(path, profile);
    }

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
