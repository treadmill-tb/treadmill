use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{Level, event};
use uuid::Uuid;

use crate::oci_store::ImageStore;
use crate::workdirs::JobWorkdirs;

const MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

pub async fn collect(workdirs: &JobWorkdirs, store: &dyn ImageStore) -> Result<()> {
    let leases = store.leases().await.context("listing in-use leases")?;
    if leases.is_empty() {
        return Ok(());
    }

    let held = workdirs
        .held_job_ids()
        .await
        .context("listing held job working directories")?;

    for lease in leases {
        let orphaned = Uuid::parse_str(&lease).is_ok_and(|job_id| !held.contains(&job_id));
        if !orphaned {
            continue;
        }

        event!(
            Level::INFO,
            job_id = %lease,
            "Releasing the image lease of a job with no working directory",
        );
        store
            .unpin(&lease)
            .await
            .with_context(|| format!("releasing the in-use lease of {lease}"))?;
    }

    Ok(())
}

pub fn spawn_reaper(workdirs: Arc<JobWorkdirs>, store: Arc<dyn ImageStore>, interval: Duration) {
    let interval = interval.max(MIN_SWEEP_INTERVAL);
    tokio::spawn(async move {
        loop {
            if let Err(e) = collect(&workdirs, store.as_ref()).await {
                event!(Level::WARN, error = ?e, "Failed to collect in-use image leases");
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use oci_spec::image::ImageManifest;
    use treadmill_rs::image::Digest;

    use crate::oci_store::Location;
    use crate::workdirs::{AllocationRecord, RetentionConfig};

    #[derive(Debug, Default)]
    struct StubStore {
        leases: Mutex<Vec<String>>,
    }

    impl StubStore {
        fn holding(leases: &[String]) -> Self {
            StubStore {
                leases: Mutex::new(leases.to_vec()),
            }
        }

        fn held(&self) -> Vec<String> {
            self.leases.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ImageStore for StubStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<()> {
            Ok(())
        }

        async fn manifest(&self, _: &Digest) -> Result<ImageManifest> {
            Err(anyhow!("the stub store serves no manifest"))
        }

        fn blob_path(&self, digest: &Digest) -> PathBuf {
            PathBuf::from(digest.hex())
        }

        async fn pin(&self, _: &Digest, job_id: &str) -> Result<()> {
            let mut leases = self.leases.lock().unwrap();
            if !leases.iter().any(|held| held == job_id) {
                leases.push(job_id.to_string());
            }
            Ok(())
        }

        async fn unpin(&self, job_id: &str) -> Result<()> {
            self.leases.lock().unwrap().retain(|held| held != job_id);
            Ok(())
        }

        async fn leases(&self) -> Result<Vec<String>> {
            Ok(self.held())
        }
    }

    fn workdirs(state_dir: &Path) -> JobWorkdirs {
        JobWorkdirs::open(state_dir, RetentionConfig::default()).unwrap()
    }

    async fn seed_job(wd: &JobWorkdirs, job_id: Uuid) {
        let path = wd.create(job_id).await.unwrap();
        tokio::fs::write(path.join("root.qcow2"), b"disk")
            .await
            .unwrap();
        AllocationRecord::new(
            Digest::from_sha256([7u8; 32]),
            Vec::new(),
            [("root".to_string(), "root.qcow2".to_string())],
        )
        .write(&path)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_lease_outlives_its_job_but_not_its_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path());

        let live = Uuid::new_v4();
        let retired = Uuid::new_v4();
        let collected = Uuid::new_v4();

        seed_job(&wd, live).await;
        seed_job(&wd, retired).await;
        assert!(wd.retire(retired).await.unwrap());

        let store =
            StubStore::holding(&[live.to_string(), retired.to_string(), collected.to_string()]);

        collect(&wd, &store).await.unwrap();

        let held: HashSet<String> = store.held().into_iter().collect();
        assert_eq!(held, HashSet::from([live.to_string(), retired.to_string()]),);
    }

    #[tokio::test]
    async fn a_resume_hands_the_lease_over_to_the_resuming_job() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path());

        let predecessor = Uuid::new_v4();
        let successor = Uuid::new_v4();

        seed_job(&wd, predecessor).await;
        assert!(wd.retire(predecessor).await.unwrap());

        let store = StubStore::holding(&[predecessor.to_string()]);
        collect(&wd, &store).await.unwrap();
        assert_eq!(store.held(), vec![predecessor.to_string()]);

        assert!(wd.resume(predecessor, successor).await.unwrap().is_some());
        store
            .pin(&Digest::from_sha256([7u8; 32]), &successor.to_string())
            .await
            .unwrap();

        collect(&wd, &store).await.unwrap();
        assert_eq!(store.held(), vec![successor.to_string()]);
    }

    #[tokio::test]
    async fn a_reference_that_is_not_a_job_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = workdirs(tmp.path());

        let store = StubStore::holding(&["not-a-job-id".to_string()]);
        collect(&wd, &store).await.unwrap();

        assert_eq!(store.held(), vec!["not-a-job-id".to_string()]);
    }
}
