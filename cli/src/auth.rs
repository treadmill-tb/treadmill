use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use treadmill_rs::api::switchboard::AuthToken;
use xdg::BaseDirectories;

use base64::{engine::general_purpose, Engine as _};
use ssh2::Session;

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
    Ok(token.encode_for_http())
}

fn get_token_path() -> Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix("switchboard")
        .context("Failed to initialize XDG base directories")?;

    let token_path = xdg_dirs
        .place_data_file(TOKEN_FILE)
        .context("Failed to determine token file path")?;

    Ok(token_path)
}

// Attempt to read keys from the SSH agent, if available.
// Read public key files from the user's .ssh directory.
// Read keys from the config file, if present.
pub fn read_ssh_keys() -> Result<Vec<String>> {
    let mut keys = Vec::new();

    if let Ok(session) = Session::new() {
        if let Ok(mut agent) = session.agent() {
            if agent.connect().is_ok() {
                if let Ok(identities) = agent.identities() {
                    for identity in identities {
                        let pubkey = identity.blob();
                        keys.push(general_purpose::STANDARD.encode(pubkey));
                    }
                }
            }
        }
    }

    let ssh_dir = dirs::home_dir()
        .map(|home| home.join(".ssh"))
        .unwrap_or_default();
    if ssh_dir.exists() {
        for entry in fs::read_dir(ssh_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("pub") {
                let key = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read SSH key file: {:?}", path))?;
                keys.push(key.trim().to_string());
            }
        }
    }

    if let Ok(config) = crate::config::load_config(None) {
        if let Some(config_keys) = config.ssh_keys {
            keys.extend(config_keys);
        }
    }

    Ok(keys)
}
