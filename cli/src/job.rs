use anyhow::{Context as _, Result, bail};
use std::collections::{BTreeMap, HashMap};
use treadmill_rs::api::switchboard::client::{ClientError, SwitchboardClient};
use treadmill_rs::api::switchboard::hosts::{
    HostListEntry, HostRequirementsReport, HostRequirementsRequest,
};
use treadmill_rs::api::switchboard::jobs::{
    JobImageReference, JobInfo, JobLeaseExpiryAction, JobParameter, LeaseRejection, LeaseSpec,
    RestartPolicy, UpdateJobRequest,
};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, JobState};
use uuid::Uuid;

use crate::ExitError;
use crate::cli::{ExpiryAction, JobCreateArgs, JobUpdateArgs, LeaseUntil};
use crate::ctx::Ctx;
use crate::display::{HostState, describe_instant, human_duration, short};
use crate::event::Event;
use crate::state::State;
use crate::tui;

mod create_ui;
pub mod list;
pub mod status;
pub mod wait;

pub async fn create(ctx: &mut Ctx, args: &JobCreateArgs) -> Result<()> {
    let client = ctx.authenticated_client()?;

    let interactive = !args.non_interactive && ctx.interactive();
    let args = if args.image.is_none() && args.resume.is_none() && interactive {
        create_ui::run(ctx, &client, args).await?
    } else {
        args.clone()
    };

    let init_spec = match (&args.resume, &args.image) {
        (Some(job_id), _) => JobInitSpec::Resume { job_id: *job_id },
        (None, Some(set)) => JobInitSpec::ImageSet {
            set_id: resolve_image_set(&client, set).await?,
            generation: None,
        },
        (None, None) => bail!("missing `--image` or `--resume`"),
    };
    let set_id = match init_spec {
        JobInitSpec::ImageSet { set_id, .. } => Some(set_id),
        _ => None,
    };

    let request = JobRequest {
        init_spec,
        label: args.label.clone(),
        owner: None,
        restart_policy: RestartPolicy { max_restarts: 0 },
        parameters: args
            .params
            .iter()
            .map(|(key, value)| {
                let parameter = JobParameter {
                    value: value.clone(),
                    secret: false,
                };
                (key.clone(), parameter)
            })
            .collect::<HashMap<_, _>>(),
        host_cel_predicate: args.host.clone().unwrap_or_else(|| "true".to_string()),
        lease_duration: args.lease,
        lease_expiry_action: args.reclaimable.then_some(JobLeaseExpiryAction::Preempt),
        annotations: args.annotations.iter().cloned().collect(),
    };

    let enqueued = client
        .enqueue_job(&request)
        .await
        .context("enqueueing the job")?;
    let job_id = enqueued.job_id;

    ctx.state = State::update(&ctx.state_path, |state| {
        if set_id.is_some() {
            state.last_image_set = set_id;
        }
        if !args.no_set_active {
            state.active_job = Some(job_id);
        }
    })?;

    let ok = anstyle::AnsiColor::Green.on_default();
    let dim = anstyle::Style::new().dimmed();
    let active = if args.no_set_active { "" } else { " · active" };
    ctx.note(&format!(
        "{ok}✓{ok:#} Job {} {dim}· enqueued{active}{dim:#}",
        short(job_id)
    ));
    let requirements = &enqueued.host_requirements;
    ctx.event(Event::Enqueued {
        job_id,
        active: !args.no_set_active,
        host_requirements: requirements.into(),
    });

    if let Some(error) = &requirements.compile_error {
        ctx.warn(&format!("the host predicate does not compile: {error}"));
    }
    if requirements.schedulable.is_empty() {
        ctx.warn(&format!(
            "no host can run this job: {} authorized, {} matched the predicate, {} errored",
            requirements.authorized, requirements.predicate_matched, requirements.errored
        ));
        if !args.no_wait {
            client.terminate_job(job_id).await.ok();
            bail!("not waiting for a job that no host can run; terminated it");
        }
    }

    let job = if args.no_wait {
        client.get_job(job_id).await.context("reading the job")?
    } else {
        match wait::wait(ctx, &client, job_id, interactive).await? {
            wait::Outcome::Ready(job) => {
                status::report(ctx, &client, &job).await;
                *job
            }
            wait::Outcome::Detached => client.get_job(job_id).await.context("reading the job")?,
        }
    };

    if ctx.human() {
        if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            anstream::println!("{job_id}");
        }
        return Ok(());
    }
    status::output(ctx, &job)
}

fn init_spec_of(job: &JobInfo) -> JobInitSpec {
    match &job.image.reference {
        JobImageReference::ImageSet { set_id, generation } => JobInitSpec::ImageSet {
            set_id: *set_id,
            generation: Some(*generation),
        },
        JobImageReference::Image { manifest_digest } => JobInitSpec::Image {
            manifest_digest: *manifest_digest,
        },
    }
}

async fn match_hosts(
    client: &SwitchboardClient,
    predicate: &str,
    init_spec: &JobInitSpec,
) -> Result<HostRequirementsReport, ClientError> {
    client
        .match_hosts(&HostRequirementsRequest {
            host_cel_predicate: predicate.to_string(),
            init_spec: Some(init_spec.clone()),
            owner: None,
        })
        .await
}

fn compatible<'a>(
    report: &HostRequirementsReport,
    hosts: &'a [HostListEntry],
) -> Vec<&'a HostListEntry> {
    let mut compatible: Vec<&HostListEntry> = hosts
        .iter()
        .filter(|host| report.schedulable.contains(&host.host_id))
        .collect();
    compatible.sort_by_key(|host| (HostState::of(host), host.name.clone()));
    compatible
}

async fn resolve_image_set(client: &SwitchboardClient, reference: &str) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(reference) {
        return Ok(id);
    }
    let sets = client
        .list_image_sets()
        .await
        .context("listing image sets")?;
    sets.iter()
        .find(|set| set.canonical_name.as_deref() == Some(reference))
        .map(|set| set.id)
        .with_context(|| format!("no image set with canonical name {reference:?}"))
}

/// Exit status of `job update` and `job lease` when `--if-revision` names a
/// revision the job is no longer at.
const EXIT_REVISION_MISMATCH: u8 = 3;
/// Exit status of `job update` and `job lease` when the switchboard refuses the
/// lease change.
const EXIT_LEASE_REFUSED: u8 = 4;

pub async fn update(ctx: &Ctx, args: &JobUpdateArgs) -> Result<()> {
    let label = match (&args.label, args.clear_label) {
        (Some(label), _) => Some(Some(label.clone())),
        (None, true) => Some(None),
        (None, false) => None,
    };
    let mut annotations: BTreeMap<_, _> = args
        .annotations
        .iter()
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .collect();
    for key in &args.remove_annotations {
        if annotations.insert(key.clone(), None).is_some() {
            bail!("annotation {key:?} is both set and removed");
        }
    }
    let request = UpdateJobRequest {
        label,
        lease: args.lease.or(args.lease_until.map(resolve_lease_until)),
        lease_expiry_action: args.expiry_action.map(|action| match action {
            ExpiryAction::Terminate => JobLeaseExpiryAction::Terminate,
            ExpiryAction::Preempt => JobLeaseExpiryAction::Preempt,
        }),
        annotations,
    };
    send_update(
        ctx,
        args.job.as_deref(),
        request,
        args.if_revision,
        args.yes,
    )
    .await
}

pub async fn lease(
    ctx: &Ctx,
    job: Option<&str>,
    lease: LeaseSpec,
    if_revision: Option<i64>,
    yes: bool,
) -> Result<()> {
    let request = UpdateJobRequest {
        lease: Some(lease),
        ..Default::default()
    };
    send_update(ctx, job, request, if_revision, yes).await
}

/// The lease end to request, resolving one relative to now against this
/// machine's clock, to the second. An explicit time is cut to the
/// microsecond: the switchboard stores leases as Postgres intervals, and
/// refuses finer ones.
pub fn resolve_lease_until(until: LeaseUntil) -> LeaseSpec {
    use chrono::SubsecRound;
    LeaseSpec::ExpiresAt(match until {
        LeaseUntil::At(at) => at.trunc_subsecs(6),
        LeaseUntil::FromNow(offset) => (chrono::Utc::now() + offset).trunc_subsecs(0),
    })
}

/// How close to now a lease may end before a change to it needs confirming.
const LEASE_MARGIN: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

/// Where the lease of `job` ends once `lease` and `action` are applied, as the
/// switchboard computes it: `None` while the job has not started, as its
/// lease is not running yet.
fn lease_end_after(
    job: &JobInfo,
    lease: Option<LeaseSpec>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let started_at = job.started_at?;
    let duration = chrono::TimeDelta::seconds(job.lease_duration_secs);
    let duration = match lease {
        None => duration,
        Some(LeaseSpec::Set(d)) => d,
        Some(LeaseSpec::Adjust(d)) => duration + d,
        Some(LeaseSpec::ExpiresAt(at)) => at - started_at,
    };
    Some(started_at + duration.max(chrono::TimeDelta::zero()))
}

/// What becomes of `job` when a lease with `action` ends, in words.
fn after_expiry(action: JobLeaseExpiryAction) -> &'static str {
    match action {
        JobLeaseExpiryAction::Terminate => "the job is stopped",
        JobLeaseExpiryAction::Preempt => "its host may be reclaimed for another job at any time",
    }
}

/// Ask before applying a lease change that leaves the job's lease ending
/// within [`LEASE_MARGIN`]; `Ok(true)` if it asked and was told to go ahead.
async fn confirm_lease_change(
    ctx: &Ctx,
    job: &JobInfo,
    request: &UpdateJobRequest,
    yes: bool,
) -> Result<bool> {
    if matches!(job.state, JobState::Finalized) {
        return Ok(false);
    }
    let Some(end) = lease_end_after(job, request.lease) else {
        return Ok(false);
    };
    let now = chrono::Utc::now();
    if end > now + LEASE_MARGIN || yes {
        return Ok(false);
    }

    let action = request
        .lease_expiry_action
        .unwrap_or(job.lease_expiry_action);
    let warning = format!(
        "the lease of job {} would end {} {}, after which {}",
        short(job.job_id),
        if end > now {
            "at"
        } else {
            "right away, having ended at"
        },
        describe_instant(end, now),
        after_expiry(action),
    );
    if !tui::interactive() {
        bail!("{warning}; pass --yes to change it anyway");
    }
    ctx.warn(&warning);
    if !tui::ask("Change the lease anyway?").await? {
        bail!("left the lease of job {} unchanged", short(job.job_id));
    }
    Ok(true)
}

async fn send_update(
    ctx: &Ctx,
    job: Option<&str>,
    request: UpdateJobRequest,
    mut if_revision: Option<i64>,
    yes: bool,
) -> Result<()> {
    let client = ctx.authenticated_client()?;
    let job_id = ctx.resolve_job(&client, job).await?;
    let lease_changes = request.lease.is_some() || request.lease_expiry_action.is_some();

    if lease_changes {
        let job = client
            .get_job(job_id)
            .await
            .with_context(|| format!("reading job {job_id}"))?;
        // Apply what was confirmed, or nothing: a change made while asking
        // makes the patch fail its revision check.
        if confirm_lease_change(ctx, &job, &request, yes).await? {
            if_revision.get_or_insert(job.revision);
        }
    }

    let revision = match client.update_job(job_id, &request, if_revision).await {
        Ok(revision) => revision,
        Err(ClientError::Status { status: 412, .. }) => {
            return Err(ExitError::new(
                EXIT_REVISION_MISMATCH,
                format!(
                    "job {} is no longer at revision {}; nothing was changed",
                    short(job_id),
                    if_revision.unwrap_or_default()
                ),
            )
            .into());
        }
        Err(ClientError::Status { status: 409, body }) => {
            let reason = serde_json::from_str::<LeaseRejection>(&body)
                .map_or(body, |rejection| rejection.message);
            return Err(ExitError::new(
                EXIT_LEASE_REFUSED,
                format!(
                    "the lease change of job {} was refused, and nothing was changed: {reason}",
                    short(job_id)
                ),
            )
            .into());
        }
        Err(ClientError::Status { status: 400, .. }) => bail!(
            "the switchboard rejected the change of job {}: check the label, the \
             annotations (at most 64; keys of lowercase letters, digits and `._/-`) \
             and the lease",
            short(job_id)
        ),
        Err(e) => return Err(e).with_context(|| format!("updating job {job_id}")),
    };

    // The lease as the switchboard now has it, rather than as requested: it
    // floors a lease at the job's start, and may be read by another change.
    let job = if lease_changes {
        Some(
            client
                .get_job(job_id)
                .await
                .with_context(|| format!("reading job {job_id}"))?,
        )
    } else {
        None
    };

    if !ctx.human() {
        let mut output = serde_json::json!({ "job_id": job_id, "revision": revision });
        if let Some(job) = &job {
            output["lease_duration_secs"] = job.lease_duration_secs.into();
            output["lease_expires_at"] = serde_json::to_value(job.lease_expires_at)?;
            output["lease_expiry_action"] = serde_json::to_value(job.lease_expiry_action)?;
        }
        anstream::println!("{output}");
        return Ok(());
    }

    anstream::println!("Updated job {}, now at revision {revision}", short(job_id));
    if let Some(job) = &job {
        let now = chrono::Utc::now();
        let after = after_expiry(job.lease_expiry_action);
        match job.lease_expires_at {
            None => anstream::println!(
                "Lease: {} from the job's start, which is still to come",
                human_duration(chrono::TimeDelta::seconds(job.lease_duration_secs))
            ),
            Some(end) if end > now => {
                anstream::println!("Lease ends {}; then {after}", describe_instant(end, now))
            }
            Some(end) => anstream::println!("Lease ended {}; {after}", describe_instant(end, now)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Timelike};

    #[test]
    fn resolved_end_times_fit_a_postgres_interval() {
        let LeaseSpec::ExpiresAt(at) =
            resolve_lease_until(LeaseUntil::FromNow(TimeDelta::minutes(20)))
        else {
            panic!("a lease end resolves to an end time");
        };
        assert_eq!(at.nanosecond(), 0);

        let given = "2026-08-28T14:00:00.123456789Z".parse().unwrap();
        let LeaseSpec::ExpiresAt(at) = resolve_lease_until(LeaseUntil::At(given)) else {
            panic!("a lease end resolves to an end time");
        };
        assert_eq!(at.nanosecond(), 123_456_000);
    }

    fn job(started_at: Option<&str>, lease_duration_secs: i64) -> JobInfo {
        let started_at = started_at.map(|t| t.parse().unwrap());
        serde_json::from_value(serde_json::json!({
            "job_id": Uuid::nil(),
            "label": null,
            "owner": null,
            "revision": 1,
            "annotations": {},
            "state": if started_at.is_some() { "ready" } else { "queued" },
            "initializing_stage": null,
            "image": { "reference": { "type": "image", "manifest_digest": format!("sha256:{}", "0".repeat(64)) }, "resolved_digest": null },
            "predecessor": null,
            "restart_policy": { "remaining_restarts": 0 },
            "host_cel_predicate": "true",
            "parameters": {},
            "lease_duration_secs": lease_duration_secs,
            "lease_expires_at": started_at.map(|t: chrono::DateTime<chrono::Utc>| t + TimeDelta::seconds(lease_duration_secs)),
            "lease_expiry_action": "preempt",
            "queued_at": "2026-08-28T13:00:00Z",
            "started_at": started_at,
            "dispatched_on_host_id": null,
            "termination_reason": null,
            "task_exit_status": null,
            "exit_message": null,
            "job_error": null,
            "terminated_at": null,
            "services": [],
            "job_ip_address": null,
            "permissions": [],
        }))
        .unwrap()
    }

    #[test]
    fn lease_ends_follow_the_switchboard_arithmetic() {
        let at = |t: &str| t.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        let running = job(Some("2026-08-28T14:00:00Z"), 3600);
        assert_eq!(
            lease_end_after(&running, None),
            Some(at("2026-08-28T15:00:00Z"))
        );
        assert_eq!(
            lease_end_after(&running, Some(LeaseSpec::Set(TimeDelta::minutes(30)))),
            Some(at("2026-08-28T14:30:00Z"))
        );
        assert_eq!(
            lease_end_after(&running, Some(LeaseSpec::Adjust(TimeDelta::minutes(-10)))),
            Some(at("2026-08-28T14:50:00Z"))
        );
        // Floored at the job's start, as the switchboard does.
        assert_eq!(
            lease_end_after(&running, Some(LeaseSpec::Adjust(TimeDelta::hours(-2)))),
            Some(at("2026-08-28T14:00:00Z"))
        );
        assert_eq!(
            lease_end_after(
                &running,
                Some(LeaseSpec::ExpiresAt(at("2026-08-28T13:00:00Z")))
            ),
            Some(at("2026-08-28T14:00:00Z"))
        );
        // A job that has not started has no running lease to end.
        let queued = job(None, 3600);
        assert_eq!(
            lease_end_after(&queued, Some(LeaseSpec::Set(TimeDelta::zero()))),
            None
        );
    }
}
