use anyhow::Result;
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api: Api,
}

/// Api configuration.
#[derive(Debug, Deserialize)]
pub struct Api {
    pub url: String,
}

pub fn load_config(config_path: Option<&str>) -> Result<Config> {
    let config_path = config_path.unwrap_or("~/.switchboard_config.toml");
    let config_str = fs::read_to_string(expand_tilde(config_path)?)?;
    let config: Config = toml::from_str(&config_str)?;
    Ok(config)
}

fn expand_tilde<P: AsRef<Path>>(path_user_input: P) -> Result<String> {
    let p = path_user_input.as_ref().to_str().unwrap();
    Ok(shellexpand::tilde(p).into_owned())
}
