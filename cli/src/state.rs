use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use treadmill_rs::api::switchboard::jobs::JobServiceEndpoint;
use uuid::Uuid;

use crate::locked_file;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub active_job: Option<Uuid>,
    #[serde(default)]
    pub last_image_set: Option<Uuid>,
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

    /// Apply `change` to the state stored at `path`, and return the state as
    /// stored: as one atomic step, which concurrent `tml` invocations take in
    /// turn, each changing the state the one before it stored (see
    /// [`locked_file::update`]). The file is `0600` before any secret reaches
    /// it.
    pub fn update(path: &Path, change: impl FnOnce(&mut State)) -> Result<State> {
        locked_file::update(path, 0o600, |contents| {
            let mut state: State = contents
                .as_deref()
                .and_then(|contents| serde_json::from_str(contents).ok())
                .unwrap_or_default();
            change(&mut state);
            *contents = Some(serde_json::to_string_pretty(&state)?);
            Ok(state)
        })
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
        Self::update(path, |state| {
            if state
                .job_service_tokens
                .get(&job_id)
                .and_then(|services| services.get(service))
                .is_some_and(|cached| cached.token == failed_token)
                && let Some(services) = state.job_service_tokens.get_mut(&job_id)
            {
                services.remove(service);
                if services.is_empty() {
                    state.job_service_tokens.remove(&job_id);
                }
            }
        })?;
        Ok(())
    }
}
