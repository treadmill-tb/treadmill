use std::collections::BTreeMap;
use treadmill_rs::api::switchboard::hosts::{HostLeaseState, HostRequirementsReport};
use treadmill_rs::api::switchboard::jobs::{
    JobInfo, JobInitializingStage, JobListState, JobSummary,
};
use treadmill_rs::api::switchboard::{JobState, TerminationReason};
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
