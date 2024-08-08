use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use xdg::BaseDirectories;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: Api,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Api {
    pub url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api: Api {
                url: "https://api.treadmill.ci".to_string(),
            },
        }
    }
}

pub fn load_config(config_path: Option<&str>) -> Result<Config> {
    let xdg_dirs = BaseDirectories::with_prefix("switchboard")?;

    let config_path = match config_path {
        Some(path) => PathBuf::from(path),
        None => xdg_dirs.find_config_file("config.toml").unwrap_or_else(|| {
            xdg_dirs
                .place_config_file("config.toml")
                .expect("Failed to create config file")
        }),
    };

    if !config_path.exists() {
        let default_config = Config::default();
        let toml = toml::to_string(&default_config)?;
        fs::write(&config_path, toml)?;
    }

    let config_str = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {:?}", config_path))?;
    let config: Config = toml::from_str(&config_str)
        .with_context(|| format!("Failed to parse config file: {:?}", config_path))?;

    Ok(config)
}
