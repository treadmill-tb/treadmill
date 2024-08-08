use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use treadmill_rs::api::switchboard::AuthToken;

const TOKEN_FILE: &str = ".switchboard_token";

pub fn save_token(token: &AuthToken) -> Result<()> {
    let token_path = get_token_path()?;
    fs::write(token_path, serde_json::to_string(token)?)?;
    Ok(())
}

pub fn get_token() -> Result<String> {
    let token_path = get_token_path()?;
    let token: AuthToken = serde_json::from_str(&fs::read_to_string(token_path)?)?;
    Ok(serde_json::to_string(&token)?)
}

fn get_token_path() -> Result<PathBuf> {
    let mut path =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Unable to determine home directory"))?;
    path.push(TOKEN_FILE);
    Ok(path)
}
