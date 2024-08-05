use crate::herd::SupervisorActorProxy;
use crate::model::job::JobModel;
use crate::server::auth::DbPermSource;
use chrono::Utc;
use dashmap::{DashMap, Entry};
use sqlx::PgExecutor;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{watch, Mutex, OwnedMutexGuard};
use treadmill_rs::api::switchboard::JobStatus;
use treadmill_rs::api::switchboard_supervisor::{
    JobInitSpec, ParameterValue, RendezvousServerSpec, RestartPolicy, SupervisorStatus,
};
use treadmill_rs::connector::StartJobMessage;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("Job is already active")]
    JobAlreadyActive,
}
#[derive(Debug)]
pub struct JobSpec {
    job_id: Uuid,
    job_init_spec: JobInitSpec,
    restart_policy: RestartPolicy,
    tag_config: String,
    timeout: chrono::TimeDelta,
    ssh_rendezvous_servers: Vec<RendezvousServerSpec>,
    parameters: HashMap<String, ParameterValue>,
    ssh_keys: Vec<String>,
    #[allow(dead_code)]
    // TODO
    auth_source: DbPermSource,
}
impl JobSpec {
    pub fn timeout(&self) -> chrono::TimeDelta {
        self.timeout
    }
    pub fn restart_policy(&self) -> &RestartPolicy {
        &self.restart_policy
    }
    pub fn init_spec(&self) -> &JobInitSpec {
        &self.job_init_spec
    }
    pub fn tag_config(&self) -> &str {
        &self.tag_config
    }
}
pub async fn build_start_message(
    job_spec: &JobSpec,
    _db: impl PgExecutor<'_>,
) -> Result<StartJobMessage, sqlx::Error> {
    Ok(StartJobMessage {
        job_id: job_spec.job_id,
        init_spec: job_spec.job_init_spec.clone(),
        ssh_keys: job_spec.ssh_keys.clone(),
        restart_policy: job_spec.restart_policy.clone(),
        ssh_rendezvous_servers: job_spec.ssh_rendezvous_servers.clone(),
        parameters: job_spec.parameters.clone(),
    })
}

#[derive(Debug)]
pub struct JobActor {
    job_spec: JobSpec,
    inner: JobActorInner,
}
#[derive(Debug)]
enum JobActorInner {
    Dormant(()),
    Active(ActiveJob),
}
impl JobActor {
    // Warning: if calling this function, please update the known_state in the database.
    pub fn activate(
        &mut self,
        supervisor_actor_proxy: SupervisorActorProxy,
    ) -> Result<&mut ActiveJob, JobError> {
        self.inner = JobActorInner::Active(ActiveJob {
            supervisor_actor_proxy,
            started_at: Utc::now(),
        });
        Ok(match &mut self.inner {
            JobActorInner::Active(active) => active,
            _ => unreachable!(),
        })
    }
    pub fn job_spec(&self) -> &JobSpec {
        &self.job_spec
    }
    pub async fn cancel(&mut self) {
        match &mut self.inner {
            JobActorInner::Dormant(_) => (
                // TODO
            ),
            JobActorInner::Active(active_job) => {
                // if this is None, then that's fine. It'll have previously been marked as canceled,
                // so whenself the supervisor reconnects, if it has an active job, we'll cancel the job.
                let _ = active_job
                    .supervisor_actor_proxy
                    .stop_current_job(self.job_spec.job_id)
                    .await;
            }
        }
    }
    pub fn is_active(&self) -> bool {
        matches!(&self.inner, JobActorInner::Active(_))
    }
    pub async fn wait_for_supervisor_idle(&self) {
        match &self.inner {
            JobActorInner::Dormant(_) => {
                unimplemented!()
            }
            JobActorInner::Active(active_job) => {
                while let Some(status) = active_job
                    .supervisor_actor_proxy
                    .request_supervisor_status()
                    .await
                {
                    if !matches!(status, SupervisorStatus::Idle) {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    } else {
                        break;
                    }
                }
            }
        }
    }
}
#[derive(Debug)]
#[allow(dead_code)]
struct DormantJob {
    queued_at: chrono::DateTime<Utc>,
}
#[derive(Debug)]
pub struct ActiveJob {
    supervisor_actor_proxy: SupervisorActorProxy,
    #[allow(dead_code)]
    started_at: chrono::DateTime<Utc>,
}
impl ActiveJob {
    pub fn watch_status(&self) -> watch::Receiver<JobStatus> {
        self.supervisor_actor_proxy.job_status_watcher()
    }
}

#[derive(Debug, Error)]
pub enum KanbanError {
    #[error("Job already exists")]
    JobAlreadyExists,
    #[error("No such job")]
    NoSuchJob,
}
/// The `Kanban` stores the set of all jobs that have yet to run, or are running.
#[derive(Debug)]
pub struct Kanban {
    jobs: DashMap<Uuid, Arc<Mutex<JobActor>>>,
}
impl Kanban {
    pub fn new() -> Self {
        Self {
            jobs: DashMap::new(),
        }
    }

    pub fn jobs(&self) -> &DashMap<Uuid, Arc<Mutex<JobActor>>> {
        &self.jobs
    }

    pub fn register(
        &self,
        job_model: JobModel,
        params: HashMap<String, ParameterValue>,
        auth_source: DbPermSource,
    ) -> Result<(), KanbanError> {
        let id = job_model.id();
        let job_spec = JobSpec {
            job_id: id,
            job_init_spec: job_model.init_spec(),
            restart_policy: job_model.restart_policy(),
            tag_config: job_model.tag_config().to_owned(),
            timeout: job_model.timeout(),
            ssh_rendezvous_servers: job_model.rendezvous_servers(),
            parameters: params,
            ssh_keys: job_model.ssh_keys().to_vec(),
            auth_source,
        };
        match self.jobs.entry(id) {
            Entry::Occupied(_) => {
                tracing::error!("Cannot register job: job ({id}) already exists.");
                return Err(KanbanError::JobAlreadyExists);
            }
            Entry::Vacant(v) => {
                v.insert(Arc::new(Mutex::new(JobActor {
                    job_spec,
                    inner: JobActorInner::Dormant(
                        (), // DormantJob { queued_at: Utc::now(), }
                    ),
                })));
            }
        }

        Ok(())
    }

    pub fn unregister(&self, job_id: Uuid) -> Result<(), KanbanError> {
        match self.jobs.remove(&job_id) {
            None => Err(KanbanError::NoSuchJob),
            Some(_) => Ok(()),
        }
    }

    pub async fn lock_metadata(
        &self,
        job_id: Uuid,
    ) -> Result<OwnedMutexGuard<JobActor>, KanbanError> {
        Ok(self
            .jobs
            .get(&job_id)
            .map(|r| Arc::clone(r.value()))
            .ok_or(KanbanError::NoSuchJob)?
            .lock_owned()
            .await)
    }
}
