//! Per-job working directories under the supervisor's state directory.
//!
//! ```text
//! <state_dir>/supervisor.lock          exclusive flock, held for the process lifetime
//! <state_dir>/jobs/<job_id>/           live or retained job
//! <state_dir>/retired/<millis>-<job_id>/  removed job, awaiting collection
//! ```
//!
//! Removal renames the workdir into `retired/`, and a reaper deletes retired
//! entries once they are older than the configured grace period. The name of a
//! retired entry carries everything needed to age it out or to move it back.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tracing::{Level, event};
use uuid::Uuid;

const LOCK_FILE: &str = "supervisor.lock";
const JOBS_DIR: &str = "jobs";
const RETIRED_DIR: &str = "retired";

const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

fn default_grace_period() -> Duration {
    DEFAULT_GRACE_PERIOD
}

fn default_sweep_interval() -> Duration {
    DEFAULT_SWEEP_INTERVAL
}

/// How long removed job working directories are kept before deletion.
#[derive(Deserialize, Debug, Clone)]
pub struct RetentionConfig {
    /// Minimum age of a retired working directory before it is deleted.
    #[serde(with = "humantime_serde", default = "default_grace_period")]
    pub grace_period: Duration,

    /// Interval at which retired working directories are collected.
    #[serde(with = "humantime_serde", default = "default_sweep_interval")]
    pub sweep_interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        RetentionConfig {
            grace_period: DEFAULT_GRACE_PERIOD,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
        }
    }
}

/// Exclusive owner of a supervisor state directory and its job working
/// directories.
#[derive(Debug)]
pub struct JobWorkdirs {
    jobs: PathBuf,
    retired: PathBuf,
    retention: RetentionConfig,
    /// Dropping this file releases the state directory lock.
    _lock: File,
}

impl JobWorkdirs {
    /// Take exclusive ownership of `state_dir`, failing if another supervisor
    /// process holds it.
    pub fn open(state_dir: &Path, retention: RetentionConfig) -> Result<Self> {
        let jobs = state_dir.join(JOBS_DIR);
        let retired = state_dir.join(RETIRED_DIR);
        for dir in [&jobs, &retired] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let lock_path = state_dir.join(LOCK_FILE);
        let lock = File::create(&lock_path)
            .with_context(|| format!("creating {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "state directory {} is in use by another supervisor process",
                state_dir.display(),
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(anyhow!(e)).with_context(|| format!("locking {}", lock_path.display()));
            }
        }

        Ok(JobWorkdirs {
            jobs,
            retired,
            retention,
            _lock: lock,
        })
    }

    pub fn path(&self, job_id: Uuid) -> PathBuf {
        self.jobs.join(job_id.to_string())
    }

    /// Create a job's working directory, failing with
    /// [`std::io::ErrorKind::AlreadyExists`] if this supervisor already has one
    /// for that job.
    pub async fn create(&self, job_id: Uuid) -> std::io::Result<PathBuf> {
        let path = self.path(job_id);
        tokio::fs::create_dir(&path).await?;
        Ok(path)
    }

    /// Move a job's working directory aside for collection. Returns `false` if
    /// there was none.
    pub async fn retire(&self, job_id: Uuid) -> Result<bool> {
        let src = self.path(job_id);
        let dst = self.retired.join(retired_name(job_id, SystemTime::now()));
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => {
                event!(Level::INFO, ?src, ?dst, "Retired job working directory");
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow!(e))
                .with_context(|| format!("retiring {} to {}", src.display(), dst.display())),
        }
    }

    /// Retire every working directory left behind by a previous supervisor
    /// process.
    pub async fn sweep(&self) -> Result<()> {
        let mut entries = tokio::fs::read_dir(&self.jobs)
            .await
            .with_context(|| format!("reading {}", self.jobs.display()))?;

        while let Some(entry) = entries.next_entry().await? {
            let Some(job_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };

            event!(
                Level::WARN,
                %job_id,
                "Retiring job working directory left behind by a previous supervisor process",
            );
            self.retire(job_id).await?;
        }

        Ok(())
    }

    /// Delete retired working directories that are older than the grace period.
    pub async fn collect(&self) -> Result<()> {
        let cutoff =
            unix_millis(SystemTime::now()).saturating_sub(self.retention.grace_period.as_millis());

        let mut entries = tokio::fs::read_dir(&self.retired)
            .await
            .with_context(|| format!("reading {}", self.retired.display()))?;

        while let Some(entry) = entries.next_entry().await? {
            let Some(retired_at) = entry
                .file_name()
                .to_str()
                .and_then(parse_retired_name)
                .map(|(retired_at, _job_id)| retired_at)
            else {
                continue;
            };

            if retired_at <= cutoff {
                let path = entry.path();
                event!(
                    Level::INFO,
                    ?path,
                    "Collecting retired job working directory"
                );
                tokio::fs::remove_dir_all(&path)
                    .await
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }

        Ok(())
    }

    /// Run [`JobWorkdirs::collect`] for as long as the supervisor lives.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let interval = this.retention.sweep_interval.max(MIN_SWEEP_INTERVAL);
        tokio::spawn(async move {
            loop {
                if let Err(e) = this.collect().await {
                    event!(Level::WARN, error = ?e, "Failed to collect retired job working directories");
                }
                tokio::time::sleep(interval).await;
            }
        });
    }
}

fn unix_millis(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

fn retired_name(job_id: Uuid, retired_at: SystemTime) -> String {
    format!("{}-{}", unix_millis(retired_at), job_id)
}

fn parse_retired_name(name: &str) -> Option<(u128, Uuid)> {
    let (retired_at, job_id) = name.split_once('-')?;
    Some((retired_at.parse().ok()?, Uuid::parse_str(job_id).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workdirs(state_dir: &Path, grace_period: Duration) -> JobWorkdirs {
        JobWorkdirs::open(
            state_dir,
            RetentionConfig {
                grace_period,
                ..RetentionConfig::default()
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_state_dir_is_held_exclusively() {
        let tmp = tempfile::tempdir().unwrap();

        let first = workdirs(tmp.path(), Duration::ZERO);
        assert!(JobWorkdirs::open(tmp.path(), RetentionConfig::default()).is_err());

        drop(first);
        assert!(JobWorkdirs::open(tmp.path(), RetentionConfig::default()).is_ok());
    }

    #[tokio::test]
    async fn a_job_gets_one_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::ZERO);
        let job_id = Uuid::new_v4();

        let path = wd.create(job_id).await.unwrap();
        assert!(path.is_dir());
        assert_eq!(
            wd.create(job_id).await.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists,
        );
    }

    #[tokio::test]
    async fn retiring_moves_the_directory_with_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));
        let job_id = Uuid::new_v4();

        let path = wd.create(job_id).await.unwrap();
        tokio::fs::write(path.join("overlay.qcow2"), b"disk")
            .await
            .unwrap();

        assert!(wd.retire(job_id).await.unwrap());
        assert!(!path.exists());
        assert!(!wd.retire(job_id).await.unwrap());

        // Within the grace period, the retired copy survives collection.
        wd.collect().await.unwrap();
        let retired = retired_entries(tmp.path());
        assert_eq!(retired.len(), 1);
        assert!(retired[0].ends_with(&job_id.to_string()));
        assert!(
            tokio::fs::read(
                tmp.path()
                    .join(RETIRED_DIR)
                    .join(&retired[0])
                    .join("overlay.qcow2")
            )
            .await
            .unwrap()
                == b"disk"
        );
    }

    #[tokio::test]
    async fn collection_honours_the_grace_period() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));

        let fresh = Uuid::new_v4();
        let stale = Uuid::new_v4();
        let retired = tmp.path().join(RETIRED_DIR);
        for (job_id, retired_at) in [
            (fresh, SystemTime::now()),
            (stale, SystemTime::now() - Duration::from_secs(7200)),
        ] {
            std::fs::create_dir(retired.join(retired_name(job_id, retired_at))).unwrap();
        }
        std::fs::create_dir(retired.join("not-a-retired-workdir")).unwrap();

        wd.collect().await.unwrap();

        let remaining = retired_entries(tmp.path());
        assert_eq!(remaining.len(), 2, "{remaining:?}");
        assert!(remaining.iter().any(|n| n.ends_with(&fresh.to_string())));
        assert!(remaining.iter().any(|n| n == "not-a-retired-workdir"));
    }

    #[tokio::test]
    async fn a_sweep_retires_leftovers_from_a_previous_process() {
        let tmp = tempfile::tempdir().unwrap();
        let job_id = Uuid::new_v4();

        let previous = workdirs(tmp.path(), Duration::ZERO);
        previous.create(job_id).await.unwrap();
        std::fs::create_dir(tmp.path().join(JOBS_DIR).join("not-a-job")).unwrap();
        drop(previous);

        let wd = workdirs(tmp.path(), Duration::ZERO);
        wd.sweep().await.unwrap();

        assert!(!wd.path(job_id).exists());
        assert!(tmp.path().join(JOBS_DIR).join("not-a-job").exists());
        assert!(retired_entries(tmp.path())[0].ends_with(&job_id.to_string()));

        // A zero grace period makes the next collection delete it.
        wd.collect().await.unwrap();
        assert!(retired_entries(tmp.path()).is_empty());
    }

    fn retired_entries(state_dir: &Path) -> Vec<String> {
        std::fs::read_dir(state_dir.join(RETIRED_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().to_string())
            .collect()
    }
}
