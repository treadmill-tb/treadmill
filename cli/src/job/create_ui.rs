use anyhow::{Context as _, Result, bail};
use chrono::TimeDelta;
use crossterm::event::KeyCode;
use futures_util::future::BoxFuture;
use std::time::Duration;
use tokio::time::Instant;
use treadmill_rs::api::switchboard::JobInitSpec;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::hosts::{HostListEntry, HostRequirementsReport};
use treadmill_rs::api::switchboard::images::ImageSetInfo;
use uuid::Uuid;

use super::{compatible, match_hosts};
use crate::cli::{JobCreateArgs, parse_duration};
use crate::ctx::Ctx;
use crate::display::{
    self, HostState, boards, image_name, is_system, program, shell_quote, with_boards,
};
use crate::tui::{
    self, DIM, ERR, Input, Item, Line, LineEdit, Terminal, WARN, answered, fit, plain, span,
};

const ANY_HOST: &str = "true";
const OVERVIEW_HOSTS: usize = 5;
const DEBOUNCE: Duration = Duration::from_millis(150);

type Match = (String, Result<HostRequirementsReport, String>);

#[derive(Clone, Copy)]
enum Step {
    Image,
    Host,
    Lease,
    Expiry,
    Label,
    Confirm,
}

pub async fn run(
    ctx: &Ctx,
    client: &SwitchboardClient,
    args: &JobCreateArgs,
) -> Result<JobCreateArgs> {
    let (sets, hosts, defaults) = tokio::try_join!(
        client.list_image_sets(),
        client.list_hosts(),
        client.job_defaults(),
    )
    .context("loading images and hosts")?;
    let default_lease = TimeDelta::seconds(defaults.lease_duration_secs);

    let mut args = args.clone();
    let lease_given = args.lease.is_some() || args.reclaimable;
    let steps: Vec<Step> = [
        Some(Step::Image),
        args.host.is_none().then_some(Step::Host),
        (!lease_given).then_some(Step::Lease),
        (!lease_given).then_some(Step::Expiry),
        args.label.is_none().then_some(Step::Label),
        Some(Step::Confirm),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut term = Terminal::open()?;
    let mut answers: Vec<Line> = Vec::new();
    let mut set = sets
        .iter()
        .find(|set| Some(set.id) == ctx.state.last_image_set);
    let mut expression = String::new();
    let mut i = 0;
    loop {
        answers.truncate(i);
        term.set_above(answers.clone());
        let answer = match steps[i] {
            Step::Image => {
                let picked = pick_image(&mut term, &sets, set.map(|set| set.id)).await?;
                set = Some(picked);
                args.image = Some(
                    picked
                        .canonical_name
                        .clone()
                        .unwrap_or_else(|| picked.id.to_string()),
                );
                Some(answered("Image", image_label(picked)))
            }
            Step::Host => {
                let init_spec = init_spec(set.context("no image chosen")?);
                pick_host(&mut term, client, &hosts, &init_spec, &mut expression)
                    .await?
                    .map(|(predicate, line)| {
                        args.host = Some(predicate);
                        answered("Host", line)
                    })
            }
            Step::Lease => pick_lease(&mut term, default_lease).await?.map(|lease| {
                args.lease = (lease != default_lease).then_some(lease);
                answered("Lease", vec![plain(duration_text(lease))])
            }),
            Step::Expiry => pick_expiry(&mut term).await?.map(|reclaimable| {
                args.reclaimable = reclaimable;
                answered(
                    "When the lease ends",
                    vec![plain(if reclaimable {
                        "Keep it running (reclaimable)"
                    } else {
                        "Stop the job"
                    })],
                )
            }),
            Step::Label => tui::input(
                &mut term,
                "Label",
                args.label.as_deref().unwrap_or(""),
                "optional",
                |_| Ok(()),
            )
            .await?
            .map(|label| {
                args.label = (!label.is_empty()).then(|| label.clone());
                answered("Label", vec![plain(args.label.as_deref().unwrap_or("—"))])
            }),
            Step::Confirm => {
                let set = set.context("no image chosen")?;
                let predicate = args.host.as_deref().unwrap_or(ANY_HOST);
                let report = match_hosts(client, predicate, &init_spec(set))
                    .await
                    .context("matching hosts")?;
                let summary = summary(&args, set, &hosts, &report, default_lease);
                term.set_above(summary.clone());
                match tui::confirm(&mut term, "Enqueue?", true).await? {
                    Some(true) => {
                        term.set_above(Vec::new());
                        term.print(&summary)?;
                        return Ok(args);
                    }
                    Some(false) => {
                        term.set_above(Vec::new());
                        term.print(&[vec![span(ERR, "✗ "), plain("Cancelled")]])?;
                        return Err(tui::interrupted());
                    }
                    None => None,
                }
            }
        };
        match answer {
            Some(line) => {
                answers.push(line);
                i += 1;
            }
            None => i = i.saturating_sub(1),
        }
    }
}

fn init_spec(set: &ImageSetInfo) -> JobInitSpec {
    JobInitSpec::ImageSet {
        set_id: set.id,
        generation: None,
    }
}

async fn pick_image<'a>(
    term: &mut Terminal,
    sets: &'a [ImageSetInfo],
    last: Option<Uuid>,
) -> Result<&'a ImageSetInfo> {
    let (system, other): (Vec<&ImageSetInfo>, Vec<&ImageSetInfo>) = sets
        .iter()
        .filter(|set| set.latest_generation.is_some())
        .partition(|set| is_system(set));
    if system.is_empty() && other.is_empty() {
        bail!("no image to run");
    }
    let listed: Vec<&ImageSetInfo> = system.iter().chain(&other).copied().collect();
    let default = listed
        .iter()
        .position(|set| Some(set.id) == last)
        .unwrap_or(0);

    let mut all = system.is_empty() || default >= system.len();
    loop {
        let mut items = image_items(&listed, if all { listed.len() } else { system.len() });
        if !all && !other.is_empty() {
            items.push(Item::action(vec![span(DIM, "All images…")]));
        }
        match tui::select(term, "Image", &[], &items, default, true).await? {
            Some(i) if i < listed.len() => return Ok(listed[i]),
            Some(_) => all = true,
            None if all && !system.is_empty() => all = false,
            None => {}
        }
    }
}

fn image_items(sets: &[&ImageSetInfo], shown: usize) -> Vec<Item> {
    let name_width = sets
        .iter()
        .map(|set| image_name(set).chars().count())
        .max()
        .unwrap_or(0);
    let canonical_width = sets
        .iter()
        .map(|set| set.canonical_name.as_deref().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    sets.iter()
        .enumerate()
        .map(|(i, set)| {
            let name = image_name(set);
            let canonical = set.canonical_name.as_deref().unwrap_or("");
            let platforms = set.platforms.join(", ");
            let search = format!("{name} {canonical} {platforms}");
            let line = vec![
                plain(fit(&name, name_width)),
                plain("  "),
                span(DIM, fit(canonical, canonical_width)),
                plain("  "),
                span(DIM, platforms),
            ];
            if i < shown {
                Item::new(line, search)
            } else {
                Item::hidden(line, search)
            }
        })
        .collect()
}

fn image_label(set: &ImageSetInfo) -> Line {
    let mut line = vec![plain(image_name(set))];
    if let Some(generation) = set.latest_generation {
        line.push(span(DIM, format!(" v{generation}")));
    }
    line
}

fn host_row(host: &HostListEntry, name_width: usize) -> Line {
    let state = HostState::of(host);
    let boards = boards(host);
    vec![
        plain(fit(&host.name, name_width)),
        plain("  "),
        span(state.style(), fit(state.as_str(), 11)),
        plain("  "),
        if boards.is_empty() {
            span(DIM, "no boards")
        } else {
            plain(boards)
        },
    ]
}

fn hosts_text(count: usize) -> String {
    match count {
        1 => "1 host".to_string(),
        n => format!("{n} hosts"),
    }
}

fn overview(report: &HostRequirementsReport, hosts: &[HostListEntry]) -> Vec<Line> {
    if let Some(error) = &report.compile_error {
        return vec![vec![span(ERR, format!("  {error}"))]];
    }
    let mut head = vec![
        plain("  "),
        span(tui::BOLD, "Hosts"),
        plain(format!(
            "  {} of {}",
            report.schedulable.len(),
            report.authorized
        )),
    ];
    if report.schedulable.is_empty() {
        head.push(span(
            WARN,
            match report.predicate_matched {
                0 => " · no match".to_string(),
                n => format!(" · {n} matched, none can run this image"),
            },
        ));
    }

    let mut lines = vec![head];
    let compatible = compatible(report, hosts);
    let shown = &compatible[..compatible.len().min(OVERVIEW_HOSTS)];
    let name_width = shown
        .iter()
        .map(|host| host.name.chars().count())
        .max()
        .unwrap_or(0);
    for host in shown {
        let mut line = vec![plain(" ".repeat(9))];
        line.extend(host_row(host, name_width));
        lines.push(line);
    }
    if compatible.len() > shown.len() {
        lines.push(vec![span(
            DIM,
            format!("{}… {} more", " ".repeat(9), compatible.len() - shown.len()),
        )]);
    }
    if report.errored > 0 {
        let first = report
            .errors
            .first()
            .map_or(String::new(), |e| format!(": {}: {}", e.name, e.message));
        lines.push(vec![span(
            WARN,
            format!("  {} errored{first}", hosts_text(report.errored as usize)),
        )]);
    }
    lines
}

fn single_host(host_id: Uuid) -> String {
    format!("host.id == \"{host_id}\"")
}

async fn pick_host(
    term: &mut Terminal,
    client: &SwitchboardClient,
    hosts: &[HostListEntry],
    init_spec: &JobInitSpec,
    expression: &mut String,
) -> Result<Option<(String, Line)>> {
    let report = match_hosts(client, ANY_HOST, init_spec)
        .await
        .context("matching hosts")?;
    let header = overview(&report, hosts);
    let options = [
        Item::action(vec![plain("Any compatible host")]),
        Item::action(vec![plain("A specific host…")]),
        Item::action(vec![plain("Match hosts with a CEL expression…")]),
    ];
    loop {
        match tui::select(term, "Host", &header, &options, 0, false).await? {
            Some(0) => {
                let line = vec![
                    plain("Any compatible host"),
                    span(DIM, format!(" · {}", hosts_text(report.schedulable.len()))),
                ];
                return Ok(Some((ANY_HOST.to_string(), line)));
            }
            Some(1) => {
                if let Some(host) = pick_specific(term, &report, hosts).await? {
                    return Ok(Some((single_host(host.host_id), with_boards(host))));
                }
            }
            Some(_) => {
                if let Some((predicate, count)) =
                    cel_input(term, client, hosts, init_spec, expression).await?
                {
                    let line = vec![
                        plain(predicate.clone()),
                        span(DIM, format!(" · {}", hosts_text(count))),
                    ];
                    return Ok(Some((predicate, line)));
                }
            }
            None => return Ok(None),
        }
    }
}

async fn pick_specific<'a>(
    term: &mut Terminal,
    report: &HostRequirementsReport,
    hosts: &'a [HostListEntry],
) -> Result<Option<&'a HostListEntry>> {
    let compatible = compatible(report, hosts);
    let name_width = compatible
        .iter()
        .map(|host| host.name.chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<Item> = compatible
        .iter()
        .map(|host| {
            let search = format!(
                "{} {} {}",
                host.name,
                HostState::of(host).as_str(),
                boards(host)
            );
            Item::new(host_row(host, name_width), search)
        })
        .collect();
    Ok(tui::select(term, "Host", &[], &items, 0, true)
        .await?
        .map(|i| compatible[i]))
}

async fn cel_input(
    term: &mut Terminal,
    client: &SwitchboardClient,
    hosts: &[HostListEntry],
    init_spec: &JobInitSpec,
    text: &mut String,
) -> Result<Option<(String, usize)>> {
    let mut edit = LineEdit::new(text);
    let mut report: Option<Match> = None;
    let mut deadline = (!text.trim().is_empty()).then(Instant::now);
    let mut pending: Option<BoxFuture<'_, Match>> = None;
    let mut submit = false;
    loop {
        let current = edit.text();
        if submit && deadline.is_none() && pending.is_none() {
            submit = false;
            if let Some((predicate, Ok(r))) = &report
                && *predicate == current
                && r.compile_error.is_none()
                && !r.schedulable.is_empty()
            {
                *text = current.clone();
                return Ok(Some((current, r.schedulable.len())));
            }
        }

        let (line, col) = tui::prompt_line("CEL expression", &edit, "e.g. host.site == \"lab\"");
        let mut lines = vec![line];
        if !current.trim().is_empty() {
            match &report {
                Some((_, Ok(r))) => lines.extend(overview(r, hosts)),
                Some((_, Err(error))) => lines.push(vec![span(ERR, format!("  {error}"))]),
                None => lines.push(vec![span(DIM, "  checking…")]),
            }
        }
        term.draw(&lines, Some((0, col)))?;

        tokio::select! {
            input = term.input() => match input? {
                Input::Interrupt => return Err(tui::interrupted()),
                Input::Resize => {}
                Input::Key(key) => match key.code {
                    KeyCode::Esc => {
                        *text = current;
                        return Ok(None);
                    }
                    KeyCode::Enter => submit = true,
                    _ => {
                        if edit.handle(&key) {
                            deadline = Some(Instant::now() + DEBOUNCE);
                            pending = None;
                            submit = false;
                        }
                    }
                },
            },
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                if current.trim().is_empty() {
                    report = None;
                } else {
                    pending = Some(Box::pin(async move {
                        let result = match_hosts(client, &current, init_spec)
                            .await
                            .map_err(|e| e.to_string());
                        (current, result)
                    }));
                }
            },
            result = async { pending.as_mut().expect("guarded by is_some").await }, if pending.is_some() => {
                pending = None;
                report = Some(result);
            },
        }
    }
}

async fn pick_lease(term: &mut Terminal, default: TimeDelta) -> Result<Option<TimeDelta>> {
    let mut options = vec![default];
    for hours in [1, 4] {
        if TimeDelta::hours(hours) != default {
            options.push(TimeDelta::hours(hours));
        }
    }
    let mut items: Vec<Item> = options
        .iter()
        .map(|lease| Item::action(vec![plain(duration_text(*lease))]))
        .collect();
    items[0].line.push(span(DIM, " (default)"));
    items.push(Item::action(vec![plain("Other…")]));

    loop {
        match tui::select(term, "Lease", &[], &items, 0, false).await? {
            Some(i) if i < options.len() => return Ok(Some(options[i])),
            Some(_) => {
                let validate = |text: &str| parse_duration(text).map(|_| ());
                if let Some(text) = tui::input(term, "Lease", "", "e.g. 45m", validate).await? {
                    return Ok(Some(parse_duration(&text).map_err(anyhow::Error::msg)?));
                }
            }
            None => return Ok(None),
        }
    }
}

async fn pick_expiry(term: &mut Terminal) -> Result<Option<bool>> {
    let expiry = [
        Item::action(vec![plain("Stop the job")]),
        Item::action(vec![
            plain("Keep it running until its host is needed"),
            span(DIM, " (reclaimable)"),
        ]),
    ];
    Ok(
        tui::select(term, "When the lease ends", &[], &expiry, 0, false)
            .await?
            .map(|i| i == 1),
    )
}

fn duration_text(duration: TimeDelta) -> String {
    match duration.num_seconds() {
        0 => "0s".to_string(),
        secs if secs % 3600 == 0 => format!("{}h", secs / 3600),
        secs if secs % 60 == 0 => format!("{}m", secs / 60),
        secs => format!("{secs}s"),
    }
}

fn summary(
    args: &JobCreateArgs,
    set: &ImageSetInfo,
    hosts: &[HostListEntry],
    report: &HostRequirementsReport,
    default_lease: TimeDelta,
) -> Vec<Line> {
    let predicate = args.host.as_deref().unwrap_or(ANY_HOST);
    let host = if predicate == ANY_HOST {
        "Any compatible host".to_string()
    } else {
        hosts
            .iter()
            .find(|host| single_host(host.host_id) == predicate)
            .map_or_else(|| predicate.to_string(), |host| host.name.clone())
    };
    let count = report.schedulable.len();
    let lease = format!(
        "{} · {}",
        duration_text(args.lease.unwrap_or(default_lease)),
        if args.reclaimable {
            "reclaimable"
        } else {
            "stop"
        }
    );

    let mut rows = vec![
        ("Image", image_label(set)),
        (
            "Host",
            vec![
                plain(host),
                span(
                    if count == 0 { WARN } else { DIM },
                    format!(" · {}", hosts_text(count)),
                ),
            ],
        ),
        ("Lease", vec![plain(lease)]),
        ("Label", vec![plain(args.label.as_deref().unwrap_or("—"))]),
    ];
    if !args.params.is_empty() {
        rows.push(("Parameters", vec![plain(pairs(&args.params))]));
    }
    if !args.annotations.is_empty() {
        rows.push(("Annotations", vec![plain(pairs(&args.annotations))]));
    }

    let mut lines = vec![Vec::new()];
    lines.extend(display::table(&rows, &[]));
    lines.push(Vec::new());
    lines.push(vec![span(DIM, "  $ "), plain(command_line(args))]);
    lines.push(vec![]);
    lines
}

fn pairs(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn command_line(args: &JobCreateArgs) -> String {
    let mut words = vec![format!("{} job create", program())];
    let mut push = |flag: &str, value: Option<&str>| {
        words.push(flag.to_string());
        if let Some(value) = value {
            words.push(shell_quote(value));
        }
    };
    if let Some(image) = &args.image {
        push("--image", Some(image));
    }
    if let Some(host) = args.host.as_deref().filter(|host| *host != ANY_HOST) {
        push("--host", Some(host));
    }
    if let Some(lease) = args.lease {
        push("--lease", Some(&duration_text(lease)));
    }
    if args.reclaimable {
        push("--reclaimable", None);
    }
    if let Some(label) = &args.label {
        push("--label", Some(label));
    }
    for (key, value) in &args.params {
        push("--param", Some(&format!("{key}={value}")));
    }
    for (key, value) in &args.annotations {
        push("--annotation", Some(&format!("{key}={value}")));
    }
    if args.no_wait {
        push("--no-wait", None);
    }
    if args.no_set_active {
        push("--no-set-active", None);
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_texts_parse_back() {
        for lease in [
            TimeDelta::seconds(90),
            TimeDelta::minutes(30),
            TimeDelta::hours(4),
        ] {
            assert_eq!(parse_duration(&duration_text(lease)), Ok(lease));
        }
        assert_eq!(duration_text(TimeDelta::minutes(90)), "90m");
    }
}
