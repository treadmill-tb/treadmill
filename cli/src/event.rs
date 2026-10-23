use std::collections::BTreeMap;
use treadmill_rs::api::switchboard::hosts::{
    HostInfo, HostLeaseState, HostListEntry, HostRequirementsReport,
};
use treadmill_rs::api::switchboard::jobs::{
    JobInfo, JobInitializingStage, JobLeaseExpiryAction, JobListState, JobSummary,
};
use treadmill_rs::api::switchboard::{JobState, TerminationReason, WhoAmIResponse};
use uuid::Uuid;

use crate::cli::OutputFormat;
use crate::display::HostState;

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event<'a> {
    Enqueued {
        job_id: Uuid,
        active: bool,
        host_requirements: HostCounts,
    },
    Warning {
        message: &'a str,
    },
    State {
        state: JobState,
        initializing_stage: Option<JobInitializingStage>,
    },
    Hosts {
        compatible: BTreeMap<HostState, usize>,
    },
    Assigned {
        host_id: Uuid,
        host_name: Option<&'a str>,
        boards: Vec<&'a str>,
    },
    Ready,
    Cancelled {
        job_id: Uuid,
    },
    Ended {
        state: JobState,
        termination_reason: Option<TerminationReason>,
        job_error: Option<&'a str>,
    },
    TerminationRequested {
        job_id: Uuid,
    },
    Updated {
        job_id: Uuid,
        revision: i64,
        /// The job's lease as the switchboard has it after a lease change.
        #[serde(flatten)]
        lease: Option<Lease>,
    },
    Identity {
        profile: &'a str,
        switchboard: &'a str,
        #[serde(flatten)]
        identity: &'a WhoAmIResponse,
    },
    LoggedOut {
        /// Whether the token was revoked on the switchboard; `false` if it
        /// was already gone, `null` if revoking it failed.
        revoked: Option<bool>,
    },
    Context {
        profile: &'a str,
        switchboard: &'a str,
        active_job: Option<Uuid>,
    },
    ActiveJob {
        active_job: Option<Uuid>,
    },
    Job {
        job: &'a JobInfo,
    },
    #[serde(rename = "job")]
    ListedJob {
        lease_state: Option<HostLeaseState>,
        job: &'a JobSummary,
    },
    Page {
        state: JobListState,
        count: usize,
        next_cursor: Option<&'a str>,
    },
    Host {
        state: HostState,
        host: &'a HostInfo,
    },
    #[serde(rename = "host")]
    ListedHost {
        state: HostState,
        host: &'a HostListEntry,
    },
    #[serde(rename = "page")]
    HostPage {
        count: usize,
        next_cursor: Option<&'a str>,
    },
}

#[derive(serde::Serialize)]
pub struct Lease {
    pub lease_duration_secs: i64,
    pub lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub lease_expiry_action: JobLeaseExpiryAction,
}

#[derive(serde::Serialize)]
pub struct HostCounts {
    authorized: u32,
    predicate_matched: u32,
    image_matched: Option<u32>,
    schedulable: usize,
    errored: u32,
}

impl From<&HostRequirementsReport> for HostCounts {
    fn from(report: &HostRequirementsReport) -> Self {
        Self {
            authorized: report.authorized,
            predicate_matched: report.predicate_matched,
            image_matched: report.image_matched,
            schedulable: report.schedulable.len(),
            errored: report.errored,
        }
    }
}

pub fn emit(output: OutputFormat, event: Event) {
    if output == OutputFormat::Jsonl
        && let Ok(line) = serde_json::to_string(&event)
    {
        anstream::println!("{line}");
    }
}

/// Print the result of a command that ends in a single message: with `-o
/// jsonl` as that message, and with `-o json` as its fields alone, without the
/// `type` tag. Human output is the command's own.
pub fn result(output: OutputFormat, event: Event) {
    match output {
        OutputFormat::Human => {}
        OutputFormat::Jsonl => emit(output, event),
        OutputFormat::Json => {
            let Ok(mut value) = serde_json::to_value(&event) else {
                return;
            };
            if let Some(fields) = value.as_object_mut() {
                fields.remove("type");
            }
            if let Ok(text) = serde_json::to_string_pretty(&value) {
                anstream::println!("{text}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_the_fields_of_the_json_they_replace() {
        let identity = WhoAmIResponse {
            user_id: Uuid::nil(),
            name: "ada".to_string(),
            admin: false,
        };
        let value = serde_json::to_value(Event::Identity {
            profile: "default",
            switchboard: "https://tml.example",
            identity: &identity,
        })
        .unwrap();
        assert_eq!(value["type"], "identity");
        assert_eq!(value["name"], "ada");
        assert_eq!(value["user_id"], Uuid::nil().to_string());

        let updated = serde_json::to_value(Event::Updated {
            job_id: Uuid::nil(),
            revision: 3,
            lease: None,
        })
        .unwrap();
        assert_eq!(
            updated,
            serde_json::json!({ "type": "updated", "job_id": Uuid::nil(), "revision": 3 })
        );
    }
}
