use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use treadmill_rs::api::switchboard::AuthToken;
use xdg::BaseDirectories;

const TOKEN_FILE: &str = "token.json";

pub fn save_token(token: &AuthToken) -> Result<()> {
    let token_path = get_token_path()?;
    fs::write(&token_path, serde_json::to_string(token)?)
        .with_context(|| format!("Failed to write token to {:?}", token_path))?;
    Ok(())
}

pub fn get_token() -> Result<String> {
    let token_path = get_token_path()?;
    let token_str = fs::read_to_string(&token_path)
        .with_context(|| format!("Failed to read token from {:?}", token_path))?;
    let token: AuthToken =
        serde_json::from_str(&token_str).with_context(|| "Failed to parse token JSON")?;
    Ok(serde_json::to_string(&token)?)
}

fn get_token_path() -> Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix("switchboard")
        .context("Failed to initialize XDG base directories")?;

    let token_path = xdg_dirs
        .place_data_file(TOKEN_FILE)
        .context("Failed to determine token file path")?;

    Ok(token_path)
}
