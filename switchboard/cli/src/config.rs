use anyhow::Result;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use xdg::BaseDirectories;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api: Api,
}

#[derive(Debug, Deserialize)]
pub struct Api {
    pub url: String,
}

pub fn load_config(config_path: Option<&str>) -> Result<Config> {
    let xdg_dirs = BaseDirectories::with_prefix("switchboard")?;
    let config_path = match config_path {
        Some(path) => PathBuf::from(path),
        None => xdg_dirs.place_config_file("config.toml")?.to_path_buf(),
    };

    let config_str = fs::read_to_string(&config_path)?;
    let config: Config = toml::from_str(&config_str)?;
    Ok(config)
}

// default configuration
impl Default for Config {
    fn default() -> Self {
        Config {
            api: Api {
                url: "https://api.treadmill.ci".to_string(),
            },
        }
    }
}
