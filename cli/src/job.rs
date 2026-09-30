use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;
use treadmill_rs::api::switchboard::client::{ClientError, SwitchboardClient};
use treadmill_rs::api::switchboard::hosts::{
    HostListEntry, HostRequirementsReport, HostRequirementsRequest,
};
use treadmill_rs::api::switchboard::jobs::{
    JobImageReference, JobInfo, JobLeaseExpiryAction, JobParameter, RestartPolicy,
};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest};
use uuid::Uuid;

use crate::cli::JobCreateArgs;
use crate::ctx::Ctx;
use crate::display::{HostState, short};
use crate::event::Event;

mod create_ui;
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
        (Some(job), _) => JobInitSpec::Resume {
            job_id: ctx.resolve_job(Some(job)).await?,
        },
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

    if set_id.is_some() {
        ctx.state.last_image_set = set_id;
    }
    if !args.no_set_active {
        ctx.state.active_job = Some(job_id);
    }
    ctx.state.store(&ctx.state_path)?;

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
