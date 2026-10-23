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
            if let Some(text) = fields(&event).and_then(|v| serde_json::to_string_pretty(&v).ok()) {
                anstream::println!("{text}");
            }
        }
    }
}

/// `event` as `-o json` prints it: its fields, without the `type` tag.
fn fields(event: &Event) -> Option<serde_json::Value> {
    let mut value = serde_json::to_value(event).ok()?;
    value.as_object_mut()?.remove("type");
    Some(value)
}

#[cfg(test)]
mod tests {
    //! The `-o json` and `-o jsonl` output is an interface scripts rely on, so
    //! every message is pinned by a snapshot. Review a change with `cargo insta
    //! review`, or accept it with `INSTA_UPDATE=always cargo test`.

    use super::*;
    use insta::assert_json_snapshot;

    const JOB: Uuid = Uuid::from_u128(0x0192f3a4_5b6c_7d8e_9fa0_b1c2d3e4f5a6);
    const HOST: Uuid = Uuid::from_u128(0x0679be07_6106_48aa_8057_b1d4f2e18a99);

    fn job_info() -> JobInfo {
        serde_json::from_value(serde_json::json!({
            "job_id": JOB,
            "label": "bringup",
            "owner": { "id": Uuid::nil(), "kind": "user", "name": "ada" },
            "revision": 4,
            "annotations": { "ci.run": "17" },
            "state": "ready",
            "initializing_stage": null,
            "image": {
                "reference": { "type": "image_set", "set_id": Uuid::nil(), "generation": 2 },
                "resolved_digest": null
            },
            "predecessor": null,
            "restart_policy": { "remaining_restarts": 0 },
            "host_cel_predicate": "true",
            "parameters": {},
            "lease_duration_secs": 3600,
            "lease_expires_at": "2026-08-28T15:00:00Z",
            "lease_expiry_action": "terminate",
            "queued_at": "2026-08-28T13:59:00Z",
            "started_at": "2026-08-28T14:00:00Z",
            "dispatched_on_host_id": HOST,
            "termination_reason": null,
            "task_exit_status": null,
            "exit_message": null,
            "job_error": null,
            "terminated_at": null,
            "services": [],
            "job_ip_address": null,
            "permissions": ["read"],
        }))
        .unwrap()
    }

    fn job_summary() -> JobSummary {
        serde_json::from_value(serde_json::json!({
            "job_id": JOB,
            "label": "bringup",
            "owner": { "id": Uuid::nil(), "kind": "user", "name": "ada" },
            "revision": 4,
            "annotations": {},
            "state": "ready",
            "image": {
                "reference": { "type": "image_set", "set_id": Uuid::nil(), "generation": 2 },
                "resolved_digest": null
            },
            "image_name": "debian-trixie",
            "predecessor": null,
            "queued_at": "2026-08-28T13:59:00Z",
            "started_at": "2026-08-28T14:00:00Z",
            "terminated_at": null,
            "dispatched_on_host_id": HOST,
            "host_name": "pton-rpi2004",
            "termination_reason": null,
            "task_exit_status": null,
            "lease_expires_at": "2026-08-28T15:00:00Z",
            "lease_expiry_action": "terminate",
        }))
        .unwrap()
    }

    fn host_state() -> serde_json::Value {
        serde_json::json!({
            "host_id": HOST,
            "name": "pton-rpi2004",
            "live": true,
            "last_seen_at": "2026-08-28T14:30:00Z",
            "maintenance": false,
            "lease_state": "busy",
            "current_lease_expires_at": "2026-08-28T15:00:00Z",
            "current_lease_expiry_action": "terminate",
            "spec_revision": 3,
        })
    }

    fn host_entry() -> HostListEntry {
        let mut host = host_state();
        host["spec"] = serde_json::json!({
            "description": null,
            "site": "pton",
            "location": null,
            "platform": {
                "kind": "physical",
                "arch": "aarch64",
                "profiles": ["arm64-nbd-netboot"],
                "vendor": "Raspberry Pi",
                "model": "Raspberry Pi 5 8GB",
                "hypervisor": null
            },
            "resources": { "cpu_cores": 4, "memory_mb": 8192, "storage_gb": 32 },
            "labels": { "rack": "b2" },
            "duts": [{ "name": null, "vendor": "Raspberry Pi", "board": "pico2" }]
        });
        serde_json::from_value(host).unwrap()
    }

    fn host_info() -> HostInfo {
        let mut host = host_state();
        host["owner"] = serde_json::Value::Null;
        host["spec"] = serde_json::json!({ "spec_version": "v2", "site": "pton" });
        host["permissions"] = serde_json::json!(["read", "start"]);
        serde_json::from_value(host).unwrap()
    }

    #[test]
    fn job_creation_and_waiting() {
        let report: HostRequirementsReport = serde_json::from_value(serde_json::json!({
            "authorized": 5,
            "predicate_matched": 3,
            "image_matched": 2,
            "schedulable": [HOST],
            "errored": 0,
            "errors": [],
            "compile_error": null,
            "hosts": [],
        }))
        .unwrap();
        let job = job_info();
        assert_json_snapshot!(
            "jsonl_job_wait",
            [
                serde_json::to_value(Event::Enqueued {
                    job_id: JOB,
                    active: true,
                    host_requirements: (&report).into(),
                }),
                serde_json::to_value(Event::State {
                    state: JobState::Initializing,
                    initializing_stage: Some(JobInitializingStage::Booting),
                }),
                serde_json::to_value(Event::Hosts {
                    compatible: BTreeMap::from([(HostState::Idle, 1), (HostState::Busy, 2)]),
                }),
                serde_json::to_value(Event::Assigned {
                    host_id: HOST,
                    host_name: Some("pton-rpi2004"),
                    boards: vec!["pico2"],
                }),
                serde_json::to_value(Event::Ready),
                serde_json::to_value(Event::Ended {
                    state: JobState::Finalized,
                    termination_reason: Some(TerminationReason::ExecutionTimeout),
                    job_error: None,
                }),
                serde_json::to_value(Event::Cancelled { job_id: JOB }),
                serde_json::to_value(Event::Warning { message: "careful" }),
                serde_json::to_value(Event::Job { job: &job }),
            ]
            .map(Result::unwrap)
        );
    }

    #[test]
    fn listings() {
        let job = job_summary();
        let host = host_entry();
        assert_json_snapshot!(
            "jsonl_listings",
            [
                serde_json::to_value(Event::ListedJob {
                    lease_state: Some(HostLeaseState::Busy),
                    job: &job,
                }),
                serde_json::to_value(Event::Page {
                    state: JobListState::Active,
                    count: 1,
                    next_cursor: Some("c2"),
                }),
                serde_json::to_value(Event::ListedHost {
                    state: HostState::of(&host),
                    host: &host,
                }),
                serde_json::to_value(Event::HostPage {
                    count: 1,
                    next_cursor: None,
                }),
            ]
            .map(Result::unwrap)
        );
    }

    /// The messages a command ends in, and `-o json`'s rendering of each.
    #[test]
    fn results() {
        let identity = WhoAmIResponse {
            user_id: Uuid::nil(),
            name: "ada".to_string(),
            admin: false,
        };
        let host = host_info();
        let results = [
            Event::Identity {
                profile: "default",
                switchboard: "https://tml.example",
                identity: &identity,
            },
            Event::Context {
                profile: "default",
                switchboard: "https://tml.example",
                active_job: Some(JOB),
            },
            Event::ActiveJob { active_job: None },
            Event::LoggedOut {
                revoked: Some(true),
            },
            Event::Updated {
                job_id: JOB,
                revision: 5,
                lease: None,
            },
            Event::Updated {
                job_id: JOB,
                revision: 6,
                lease: Some(Lease {
                    lease_duration_secs: 5400,
                    lease_expires_at: Some("2026-08-28T15:30:00Z".parse().unwrap()),
                    lease_expiry_action: JobLeaseExpiryAction::Preempt,
                }),
            },
            Event::TerminationRequested { job_id: JOB },
            Event::Host {
                state: HostState::Busy,
                host: &host,
            },
        ];
        for event in &results {
            let jsonl = serde_json::to_value(event).unwrap();
            let json = fields(event).unwrap();
            let mut tagged = json.clone();
            tagged["type"] = jsonl["type"].clone();
            assert_eq!(tagged, jsonl, "-o json is -o jsonl without the type");
        }
        assert_json_snapshot!("jsonl_results", results);
    }
}
