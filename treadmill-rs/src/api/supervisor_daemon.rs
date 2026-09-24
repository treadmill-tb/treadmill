//! Types used in the interface between supervisors and the daemon running on
//! hosts.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::util::Secret;

/// How the job reaches the switchboard API, and the token it acts with there.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SwitchboardApi {
    pub base_url: String,
    pub token: Secret<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobInfo {
    pub job_id: Uuid,
    /// `None` when the supervisor has no switchboard to point the job at.
    pub api: Option<SwitchboardApi>,
}

pub const JOB_PATH: &str = "/job";
pub const JOB_READY_PATH: &str = "/job/ready";

#[cfg(feature = "client")]
pub struct SupervisorClient {
    http: reqwest::Client,
    base_url: String,
}

#[cfg(feature = "client")]
impl SupervisorClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        SupervisorClient {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn job_info(&self) -> Result<JobInfo, reqwest::Error> {
        self.http
            .get(format!("{}{JOB_PATH}", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn report_ready(&self) -> Result<(), reqwest::Error> {
        self.http
            .put(format!("{}{JOB_READY_PATH}", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
