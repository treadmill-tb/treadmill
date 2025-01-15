// auth.rs

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine; // Add Engine trait
use ssh_key::{LineEnding, PrivateKey}; // Remove unused PublicKey import
use std::fs;
use std::os::unix::fs::PermissionsExt; // Add PermissionsExt trait
use std::path::PathBuf;
use treadmill_rs::api::switchboard::AuthToken;
use xdg::BaseDirectories;

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
    Ok(xdg_dirs
        .place_data_file("token.json")
        .context("Failed to determine token file path")?)
}

// Generate a new Ed25519 key pair for jobs
pub fn generate_job_ssh_key() -> Result<(PrivateKey, String)> {
    let private_key = PrivateKey::random(&mut rand_core::OsRng, ssh_key::Algorithm::Ed25519)
        .map_err(|e| anyhow!("Failed to generate Ed25519 key: {}", e))?;

    let public_key = private_key.public_key();
    let public_key_str = public_key
        .to_openssh()
        .map_err(|e| anyhow!("Failed to convert public key to OpenSSH format: {}", e))?;

    Ok((private_key, public_key_str))
}

// Save the private key to the Treadmill data directory
pub fn save_private_key(private_key: &PrivateKey) -> Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix("treadmill-tb")
        .context("Failed to initialize XDG base directories")?;

    let key_path = xdg_dirs
        .place_data_file("ssh-key")
        .context("Failed to determine SSH key path")?;

    let openssh_private_key = private_key
        .to_openssh(LineEnding::LF)
        .map_err(|e| anyhow!("Failed to convert private key to OpenSSH format: {}", e))?;

    fs::write(&key_path, openssh_private_key)?;
    let mut perms = fs::metadata(&key_path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&key_path, perms)?;

    Ok(key_path)
}

// Read the job's SSH endpoints from the switchboard API
pub async fn get_job_ssh_endpoints(
    client: &reqwest::Client,
    config: &crate::config::Config,
    job_id: uuid::Uuid,
) -> Result<Vec<String>> {
    let token = get_token()?;

    let response = client
        .get(&format!("{}/api/v1/jobs/{}/status", config.api.url, job_id))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        let status: treadmill_rs::api::switchboard::jobs::status::Response =
            response.json().await?;
        match status {
            treadmill_rs::api::switchboard::jobs::status::Response::Ok {
                job_status: status,
                ..
            } => {
                // TODO fix
                if let Some(endpoints) = status.ssh_endpoints {
                    Ok(endpoints)
                } else {
                    Err(anyhow!("No SSH endpoints available for this job"))
                }
            }
            _ => Err(anyhow!("Failed to get job status")),
        }
    } else {
        Err(anyhow!("Failed to get job status: {}", response.status()))
    }
}
