use anyhow::{anyhow, bail, Context, Result};
use ssh_key::{PrivateKey, PublicKey};
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
    match std::env::var("TML_API_TOKEN") {
        Ok(token_b64) => {
            let token_bytes = base64::engine::general_purpose::STANDARD
                .decode(token_b64)
                .context("Decoding Base64-encoded TML_API_TOKEN variable")?;

            let token_array: [u8; 128] = token_bytes.try_into().map_err(|vec: Vec<u8>| {
                anyhow!(
                    "TML_API_TOKEN has invalid length ({} bytes instead of 128 bytes)",
                    vec.len()
                )
            })?;

            Ok(AuthToken(token_array).encode_for_http())
        }

        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("Supplied TML_API_TOKEN is not valid UTF-8");
        }

        Err(std::env::VarError::NotPresent) => {
            let token_path = get_token_path()?;
            let token_str = fs::read_to_string(&token_path)
                .with_context(|| format!("Failed to read token from {:?}", token_path))?;
            let token: AuthToken =
                serde_json::from_str(&token_str).with_context(|| "Failed to parse token JSON")?;
            Ok(token.encode_for_http())
        }
    }
}

fn get_token_path() -> Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix("treadmill-tb")
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

    // 1. Attempt to read from SSH agent
    if let Ok(session) = Session::new() {
        if let Ok(mut agent) = session.agent() {
            if agent.connect().is_ok() {
                if let Ok(identities) = agent.identities() {
                    for identity in identities {
                        let pubkey = identity.blob();
                        // store as raw base64 (same as existing code)
                        keys.push(general_purpose::STANDARD.encode(pubkey));
                    }
                }
            }
        }
    }

    // 2. Attempt to read .pub files from ~/.ssh
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

    // 3. Attempt to read from config file
    if let Ok(config) = crate::config::load_config(None) {
        if let Some(config_keys) = config.ssh_keys {
            keys.extend(config_keys);
        }
    }

    // 4. Attempt to read the Treadmill-managed private key, if it exists
    //    Then extract and append the public portion in OpenSSH format.
    let xdg_dirs = xdg::BaseDirectories::with_prefix("treadmill-tb")
        .context("Failed to initialize XDG base directories")?;
    let treadmill_private_key_path = xdg_dirs
        .place_data_file("ssh-key")
        .context("Failed to place treadmill ssh-key file")?;

    if treadmill_private_key_path.exists() {
        let private_key_bytes = fs::read(&treadmill_private_key_path).with_context(|| {
            format!(
                "Failed to read treadmill private key: {:?}",
                treadmill_private_key_path
            )
        })?;
        let private_key = PrivateKey::from_openssh(&private_key_bytes)
            .context("Failed to parse treadmill private key")?;

        let public_key: PublicKey = private_key.public_key().clone();
        // Convert public key to OpenSSH format.
        // NOTE: `to_openssh` returns Result<String, ssh_key::Error>.
        let pub_openssh = public_key
            .to_openssh()
            .context("Failed to convert treadmill public key to OpenSSH format")?;

        // Clean it up and push it
        keys.push(pub_openssh.trim().to_string());
    }

    Ok(keys)
}
