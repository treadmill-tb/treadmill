use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use treadmill_rs::api::switchboard::jobs::JobServiceEndpoint;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub active_job: Option<Uuid>,
    #[serde(default)]
    pub job_service_tokens: BTreeMap<Uuid, BTreeMap<String, CachedJobServiceToken>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedJobServiceToken {
    pub endpoints: Vec<JobServiceEndpoint>,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(serde_json::from_str(&contents).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write via a sibling temporary file so an interrupted write cannot leave
    /// a half-written credential behind, and so the `0600` mode is in place
    /// before any secret reaches the filesystem.
    pub fn store(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("state path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

        let tmp: PathBuf = path.with_extension("json.tmp");
        let contents = serde_json::to_vec_pretty(self)?;
        write_private(&tmp, &contents).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn token_valid(&self) -> bool {
        self.token.is_some() && self.expires_at.is_none_or(|at| at > Utc::now())
    }

    pub fn valid_job_service_token(
        &self,
        job_id: Uuid,
        service: &str,
    ) -> Option<&CachedJobServiceToken> {
        self.job_service_tokens
            .get(&job_id)?
            .get(service)
            .filter(|cached| cached.expires_at > Utc::now())
    }

    /// Remove a credential whose WebSocket handshake failed.
    pub fn invalidate_job_service_token(
        path: &Path,
        job_id: Uuid,
        service: &str,
        failed_token: &str,
    ) -> Result<()> {
        let mut state = Self::load(path)?;
        if state
            .job_service_tokens
            .get(&job_id)
            .and_then(|services| services.get(service))
            .is_some_and(|cached| cached.token == failed_token)
        {
            if let Some(services) = state.job_service_tokens.get_mut(&job_id) {
                services.remove(service);
                if services.is_empty() {
                    state.job_service_tokens.remove(&job_id);
                }
            }
            state.store(path)?;
        }
        Ok(())
    }
}

fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}
