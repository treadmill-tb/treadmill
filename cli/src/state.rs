use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
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
    ///
    /// A stored state that does not parse is changed as if it were empty, but
    /// is first copied aside, so that what it held is not lost.
    pub fn update(path: &Path, change: impl FnOnce(&mut State)) -> Result<State> {
        locked_file::update(path, 0o600, |contents| {
            let mut state = match contents.as_deref() {
                None => State::default(),
                Some(contents) => match serde_json::from_str(contents) {
                    Ok(state) => state,
                    Err(error) => {
                        let backup = back_up(path, contents)?;
                        let style =
                            anstyle::AnsiColor::Yellow.on_default() | anstyle::Effects::BOLD;
                        anstream::eprintln!(
                            "{style}warning:{style:#} {} is not a valid state file ({error}); \
                             replacing it, and keeping a copy at {}",
                            path.display(),
                            backup.display()
                        );
                        State::default()
                    }
                },
            };
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

/// Copy unreadable `contents` of the state file at `path` to a new file beside
/// it, readable only by the user, as they may hold a token.
fn back_up(path: &Path, contents: &str) -> Result<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.f");
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".unreadable-{stamp}"));
    let backup = path.with_file_name(name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .with_context(|| format!("backing up {} to {}", path.display(), backup.display()))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_state_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("tml-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        fs::write(&path, "{ not json").unwrap();

        let job = Uuid::nil();
        let state = State::update(&path, |state| state.active_job = Some(job)).unwrap();
        assert_eq!(state.active_job, Some(job));
        assert_eq!(State::load(&path).unwrap().active_job, Some(job));

        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry| *entry != path)
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "{ not json");
        fs::remove_dir_all(&dir).unwrap();
    }
}
