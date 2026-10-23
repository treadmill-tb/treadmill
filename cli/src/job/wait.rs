use anyhow::{Context as _, Result, bail};
use crossterm::event::KeyCode;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;
use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::hosts::HostListEntry;
use treadmill_rs::api::switchboard::jobs::JobInfo;
use uuid::Uuid;

use super::{compatible, init_spec_of, match_hosts};
use crate::cli::{JobRef, OutputFormat};
use crate::ctx::Ctx;
use crate::display::{HostState, human_duration, phase, program, ready, short, snake, with_boards};
use crate::event::Event;
use crate::tui::{self, DIM, ERR, Input, Line, OK, Terminal, WARN, plain, span};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);

pub enum Outcome {
    Ready(Box<JobInfo>),
    Detached,
}

pub async fn run(ctx: &Ctx, job: Option<&JobRef>) -> Result<()> {
    let client = ctx.authenticated_client()?;
    let job_id = ctx.resolve_job(job).await?;
    match wait(ctx, &client, job_id, ctx.interactive()).await? {
        Outcome::Ready(job) if ctx.human() => {
            super::status::report(ctx, &client, &job).await;
            if !std::io::stdout().is_terminal() {
                anstream::println!("{job_id}");
            }
        }
        Outcome::Ready(job) => super::status::output(ctx, &job)?,
        Outcome::Detached => {}
    }
    Ok(())
}

pub async fn wait(
    ctx: &Ctx,
    client: &SwitchboardClient,
    job_id: Uuid,
    interactive: bool,
) -> Result<Outcome> {
    if interactive {
        live(ctx, client, job_id).await
    } else {
        tokio::select! {
            job = plain_wait(ctx, client, job_id) => Ok(Outcome::Ready(Box::new(job?))),
            _ = tokio::signal::ctrl_c() => {
                client.terminate_job(job_id).await.ok();
                ctx.event(Event::Cancelled { job_id });
                bail!("interrupted; termination of job {job_id} was requested");
            }
        }
    }
}

fn ended(job: &JobInfo) -> anyhow::Error {
    anyhow::anyhow!(
        "job {} ended before becoming ready: {}{}",
        job.job_id,
        job.termination_reason
            .map_or("terminating".to_string(), |reason| snake(&reason)),
        job.job_error
            .as_ref()
            .map_or(String::new(), |error| format!(" ({error})")),
    )
}

async fn plain_wait(ctx: &Ctx, client: &SwitchboardClient, job_id: Uuid) -> Result<JobInfo> {
    let jsonl = ctx.output == OutputFormat::Jsonl;
    let mut last = None;
    let mut assigned = false;
    let mut last_counts = None;
    loop {
        let job = client.get_job(job_id).await.context("reading the job")?;
        let progress = phase(&job);
        if last.as_ref() != Some(&progress) {
            ctx.note(&format!("  {progress}"));
            ctx.event(Event::State {
                state: job.state,
                initializing_stage: job.initializing_stage,
            });
            last = Some(progress);
        }
        if jsonl && job.state == JobState::Queued {
            let hosts = client.list_hosts().await.context("listing hosts")?;
            let counts = compatible_counts(client, &job, &hosts).await?;
            if last_counts.as_ref() != Some(&counts) {
                ctx.event(Event::Hosts {
                    compatible: counts.clone(),
                });
                last_counts = Some(counts);
            }
        }
        if let Some(host_id) = job.dispatched_on_host_id
            && jsonl
            && !assigned
        {
            assigned = true;
            let hosts = client.list_hosts().await.context("listing hosts")?;
            let host = hosts.iter().find(|host| host.host_id == host_id);
            let boards: Vec<&str> = host
                .and_then(|host| host.spec.as_ref())
                .map_or(Vec::new(), |spec| {
                    spec.duts.iter().map(|dut| dut.board.as_str()).collect()
                });
            ctx.event(Event::Assigned {
                host_id,
                host_name: host.map(|host| host.name.as_str()),
                boards,
            });
        }
        if ready(&job) {
            ctx.event(Event::Ready);
            return Ok(job);
        }
        if matches!(job.state, JobState::Terminating | JobState::Finalized) {
            ctx.event(Event::Ended {
                state: job.state,
                termination_reason: job.termination_reason,
                job_error: job.job_error.as_deref(),
            });
            return Err(ended(&job));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn live(ctx: &Ctx, client: &SwitchboardClient, job_id: Uuid) -> Result<Outcome> {
    let mut hosts = client.list_hosts().await.context("listing hosts")?;
    let mut term = Terminal::open()?;
    let mut tick = tokio::time::interval(TICK);
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    let mut frame = 0;
    let mut status = vec![plain("loading")];
    let mut assigned = false;
    let started = std::time::Instant::now();
    let mut first = true;

    loop {
        tokio::select! {
            input = term.input() => match input? {
                Input::Interrupt => {
                    client
                        .terminate_job(job_id)
                        .await
                        .with_context(|| format!("requesting termination of job {job_id}"))?;
                    term.print(&[vec![
                        span(ERR, "✗ "),
                        plain("Cancelled"),
                        span(DIM, format!(" · {}", short(job_id))),
                    ]])?;
                    return Err(tui::interrupted());
                }
                Input::Key(key) if key.code == KeyCode::Char('d') => {
                    term.print(&[
                        vec![
                            span(OK, "✓ "),
                            plain("Detached"),
                            span(DIM, format!(" · {}", short(job_id))),
                        ],
                        vec![span(DIM, "  $ "), plain(format!("{} job wait{}", program(), ctx.job_flag(job_id)))],
                    ])?;
                    return Ok(Outcome::Detached);
                }
                _ => {}
            },
            _ = tick.tick() => frame += 1,
            _ = poll.tick() => {
                let job = client.get_job(job_id).await.context("reading the job")?;
                if let Some(host_id) = job.dispatched_on_host_id && !assigned {
                    assigned = true;
                    if !hosts.iter().any(|h| h.host_id == host_id) {
                        hosts = client.list_hosts().await.context("listing hosts")?;
                    }
                    term.print(&[assigned_line(host_id, &hosts)])?;
                }
                if ready(&job) {
                    let mut line = vec![span(OK, "✓ "), plain("Ready")];
                    if !first {
                        let waited = chrono::TimeDelta::from_std(started.elapsed()).unwrap_or_default();
                        line.push(span(DIM, format!(" · waited {}", human_duration(waited))));
                    }
                    term.print(&[line])?;
                    return Ok(Outcome::Ready(Box::new(job)));
                }
                if matches!(job.state, JobState::Terminating | JobState::Finalized) {
                    return Err(ended(&job));
                }
                first = false;
                status = vec![plain(phase(&job))];
                if job.state == JobState::Queued {
                    hosts = client.list_hosts().await.context("listing hosts")?;
                    status.extend(queue_summary(client, &job, &hosts).await?);
                }
            },
        }

        let mut line: Line = vec![
            span(tui::ACCENT, tui::SPINNER[frame % tui::SPINNER.len()]),
            plain(" "),
        ];
        line.extend(status.iter().cloned());
        let hint = vec![span(
            DIM,
            "  ctrl-c terminates the job · d detaches and leaves it running",
        )];
        term.draw(&[line, hint], None)?;
    }
}

fn assigned_line(host_id: Uuid, hosts: &[HostListEntry]) -> Line {
    let mut line = vec![span(OK, "✓ "), plain("Assigned · ")];
    match hosts.iter().find(|host| host.host_id == host_id) {
        Some(host) => line.extend(with_boards(host)),
        None => line.push(plain(short(host_id))),
    }
    line
}

async fn queue_summary(
    client: &SwitchboardClient,
    job: &JobInfo,
    hosts: &[HostListEntry],
) -> Result<Line> {
    let counts = compatible_counts(client, job, hosts).await?;
    if counts.is_empty() {
        return Ok(vec![span(WARN, " · no compatible hosts")]);
    }
    let text = counts
        .iter()
        .map(|(state, n)| format!("{n} {}", state.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(vec![span(DIM, format!(" · {text}"))])
}

async fn compatible_counts(
    client: &SwitchboardClient,
    job: &JobInfo,
    hosts: &[HostListEntry],
) -> Result<BTreeMap<HostState, usize>> {
    let report = match_hosts(client, &job.host_cel_predicate, &init_spec_of(job))
        .await
        .context("matching hosts")?;
    let mut counts = BTreeMap::new();
    for host in compatible(&report, hosts) {
        *counts.entry(HostState::of(host)).or_default() += 1;
    }
    Ok(counts)
}
