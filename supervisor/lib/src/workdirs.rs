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

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::{Level, event};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::ImageLocation;
use treadmill_rs::image::Digest;

const LOCK_FILE: &str = "supervisor.lock";
const JOBS_DIR: &str = "jobs";
const RETIRED_DIR: &str = "retired";

pub const ALLOCATION_RECORD: &str = "allocation.json";
pub const ALLOCATION_RECORD_VERSION: u32 = 1;

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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AllocationRecord {
    pub version: u32,
    pub manifest_digest: Digest,
    pub locations: Vec<ImageLocation>,
    pub overlays: BTreeMap<String, String>,
}

impl AllocationRecord {
    pub fn new(
        manifest_digest: Digest,
        locations: Vec<ImageLocation>,
        overlays: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        AllocationRecord {
            version: ALLOCATION_RECORD_VERSION,
            manifest_digest,
            locations,
            overlays: overlays.into_iter().collect(),
        }
    }

    pub async fn write(&self, workdir: &Path) -> Result<()> {
        let path = workdir.join(ALLOCATION_RECORD);
        let bytes = serde_json::to_vec_pretty(self)
            .with_context(|| format!("serializing {}", path.display()))?;
        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("writing {}", path.display()))
    }

    pub async fn read(workdir: &Path) -> Result<Self> {
        let path = workdir.join(ALLOCATION_RECORD);
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let record: AllocationRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if record.version != ALLOCATION_RECORD_VERSION {
            bail!(
                "{} has version {}, this supervisor understands {}",
                path.display(),
                record.version,
                ALLOCATION_RECORD_VERSION,
            );
        }
        Ok(record)
    }

    pub async fn overlay(&self, workdir: &Path, role: &str) -> Result<PathBuf> {
        let name = self
            .overlays
            .get(role)
            .ok_or_else(|| anyhow!("no {role:?} overlay recorded in {ALLOCATION_RECORD}"))?;
        let path = workdir.join(name);
        let metadata = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("checking the {role:?} overlay {}", path.display()))?;
        if !metadata.is_file() || metadata.len() == 0 {
            bail!(
                "the {role:?} overlay {} is not a usable disk image",
                path.display(),
            );
        }
        Ok(path)
    }

    async fn overlays_present(&self, workdir: &Path) -> Result<()> {
        for role in self.overlays.keys() {
            self.overlay(workdir, role).await?;
        }
        Ok(())
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
    /// Take `state_dir` over for this supervisor: retire what a previous
    /// process left behind in it, and collect retired working directories for
    /// as long as this one lives.
    pub async fn start(state_dir: &Path, retention: RetentionConfig) -> Result<Arc<Self>> {
        let workdirs = Arc::new(Self::open(state_dir, retention)?);
        workdirs.sweep().await?;
        workdirs.spawn_reaper();
        Ok(workdirs)
    }

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

    pub async fn resume(&self, retired_job_id: Uuid, as_job_id: Uuid) -> Result<Option<PathBuf>> {
        let Some(src) = self.latest_retired(retired_job_id).await? else {
            return Ok(None);
        };

        let record = AllocationRecord::read(&src)
            .await
            .with_context(|| format!("reading the allocation of {}", src.display()))?;
        record
            .overlays_present(&src)
            .await
            .with_context(|| format!("checking the allocation of {}", src.display()))?;

        let dst = self.path(as_job_id);
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => {
                event!(Level::INFO, ?src, ?dst, "Resumed job working directory");
                Ok(Some(dst))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!(e))
                .with_context(|| format!("resuming {} as {}", src.display(), dst.display())),
        }
    }

    async fn latest_retired(&self, job_id: Uuid) -> Result<Option<PathBuf>> {
        let mut entries = tokio::fs::read_dir(&self.retired)
            .await
            .with_context(|| format!("reading {}", self.retired.display()))?;

        let mut latest: Option<(u128, PathBuf)> = None;
        while let Some(entry) = entries.next_entry().await? {
            let Some((retired_at, _)) = entry
                .file_name()
                .to_str()
                .and_then(parse_retired_name)
                .filter(|(_, entry_job_id)| *entry_job_id == job_id)
            else {
                continue;
            };

            if latest.as_ref().is_none_or(|(at, _)| retired_at > *at) {
                latest = Some((retired_at, entry.path()));
            }
        }

        Ok(latest.map(|(_, path)| path))
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

    pub async fn held_job_ids(&self) -> Result<HashSet<Uuid>> {
        let mut held = HashSet::new();

        let mut live = tokio::fs::read_dir(&self.jobs)
            .await
            .with_context(|| format!("reading {}", self.jobs.display()))?;
        while let Some(entry) = live.next_entry().await? {
            if let Some(job_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            {
                held.insert(job_id);
            }
        }

        let mut retired = tokio::fs::read_dir(&self.retired)
            .await
            .with_context(|| format!("reading {}", self.retired.display()))?;
        while let Some(entry) = retired.next_entry().await? {
            if let Some((_, job_id)) = entry.file_name().to_str().and_then(parse_retired_name) {
                held.insert(job_id);
            }
        }

        Ok(held)
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
                match tokio::fs::remove_dir_all(&path).await {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => {
                        return Err(anyhow!(e))
                            .with_context(|| format!("removing {}", path.display()));
                    }
                }
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

    #[tokio::test]
    async fn starting_takes_over_the_state_dir_in_one_call() {
        let tmp = tempfile::tempdir().unwrap();
        let job_id = Uuid::new_v4();

        let previous = workdirs(tmp.path(), Duration::ZERO);
        previous.create(job_id).await.unwrap();
        drop(previous);

        let wd = JobWorkdirs::start(tmp.path(), RetentionConfig::default())
            .await
            .unwrap();

        assert!(!wd.path(job_id).exists());
        assert!(retired_entries(tmp.path())[0].ends_with(&job_id.to_string()));
        assert!(JobWorkdirs::open(tmp.path(), RetentionConfig::default()).is_err());
    }

    async fn seed_job(wd: &JobWorkdirs, job_id: Uuid) -> PathBuf {
        let path = wd.create(job_id).await.unwrap();
        tokio::fs::write(path.join("root.qcow2"), b"disk")
            .await
            .unwrap();
        AllocationRecord::new(
            Digest::from_sha256([7u8; 32]),
            vec![ImageLocation {
                registry: "registry.example".to_string(),
                repository: "treadmill/image".to_string(),
            }],
            [("root".to_string(), "root.qcow2".to_string())],
        )
        .write(&path)
        .await
        .unwrap();
        path
    }

    #[tokio::test]
    async fn a_retired_job_resumes_under_a_new_id() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));
        let job_id = Uuid::new_v4();
        let successor = Uuid::new_v4();

        seed_job(&wd, job_id).await;
        assert!(wd.retire(job_id).await.unwrap());

        let resumed = wd.resume(job_id, successor).await.unwrap().unwrap();
        assert_eq!(resumed, wd.path(successor));
        assert_eq!(
            tokio::fs::read(resumed.join("root.qcow2")).await.unwrap(),
            b"disk",
        );
        assert!(retired_entries(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn a_retired_job_resumes_at_most_once() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));
        let job_id = Uuid::new_v4();

        seed_job(&wd, job_id).await;
        assert!(wd.retire(job_id).await.unwrap());

        assert!(wd.resume(job_id, Uuid::new_v4()).await.unwrap().is_some());
        assert!(wd.resume(job_id, Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resuming_a_job_this_supervisor_never_held_finds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));

        assert!(
            wd.resume(Uuid::new_v4(), Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_newest_retired_directory_of_a_job_is_the_one_resumed() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));
        let job_id = Uuid::new_v4();

        let retired = tmp.path().join(RETIRED_DIR);
        for (marker, retired_at) in [
            (b"older", SystemTime::now() - Duration::from_secs(600)),
            (b"newer", SystemTime::now()),
        ] {
            let dir = retired.join(retired_name(job_id, retired_at));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("root.qcow2"), marker).unwrap();
            AllocationRecord::new(
                Digest::from_sha256([7u8; 32]),
                Vec::new(),
                [("root".to_string(), "root.qcow2".to_string())],
            )
            .write(&dir)
            .await
            .unwrap();
        }

        let resumed = wd.resume(job_id, Uuid::new_v4()).await.unwrap().unwrap();
        assert_eq!(
            tokio::fs::read(resumed.join("root.qcow2")).await.unwrap(),
            b"newer",
        );
    }

    #[tokio::test]
    async fn a_retired_job_without_a_usable_allocation_stays_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path(), Duration::from_secs(3600));

        let no_record = Uuid::new_v4();
        wd.create(no_record).await.unwrap();
        assert!(wd.retire(no_record).await.unwrap());

        let no_overlay = Uuid::new_v4();
        let path = seed_job(&wd, no_overlay).await;
        tokio::fs::remove_file(path.join("root.qcow2"))
            .await
            .unwrap();
        assert!(wd.retire(no_overlay).await.unwrap());

        for job_id in [no_record, no_overlay] {
            assert!(wd.resume(job_id, Uuid::new_v4()).await.is_err());
            assert!(
                retired_entries(tmp.path())
                    .iter()
                    .any(|n| n.ends_with(&job_id.to_string())),
            );
        }
    }

    fn retired_entries(state_dir: &Path) -> Vec<String> {
        std::fs::read_dir(state_dir.join(RETIRED_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().to_string())
            .collect()
    }
}
