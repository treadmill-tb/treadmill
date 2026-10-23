use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::hosts::{HostInfo, HostLeaseState, HostListEntry, HostSummary};
use treadmill_rs::api::switchboard::jobs::JobLeaseExpiryAction;
use treadmill_rs::host_spec::PlatformKind;
use uuid::Uuid;

use crate::cli::{HostCommand, HostListArgs, OutputFormat};
use crate::ctx::Ctx;
use crate::display::{
    self, HostState, boards, describe_instant, human_duration, program, shell_quote, short, snake,
};
use crate::event::Event;
use crate::listing::{self, Cell, Pager, cell};
use crate::tui::{self, BOLD, DIM, Line, plain, span};

#[derive(serde::Serialize)]
struct WithState<'a, T> {
    #[serde(flatten)]
    host: &'a T,
    state: HostState,
}

pub async fn run(ctx: &Ctx, command: &HostCommand) -> Result<()> {
    let client = ctx.authenticated_client()?;
    match command {
        HostCommand::List(args) => list(ctx, &client, args).await,
        HostCommand::Show { host } => show(ctx, &client, host).await,
    }
}

async fn list(ctx: &Ctx, client: &SwitchboardClient, args: &HostListArgs) -> Result<()> {
    let terms: Vec<String> = args.terms.iter().map(|term| term.to_lowercase()).collect();
    let fetch = async || -> Result<Vec<HostListEntry>> {
        let mut hosts = client.list_hosts().await.context("listing hosts")?;
        hosts.retain(|host| matches(host, &terms));
        Ok(hosts)
    };
    if args.watch {
        return listing::watch(
            ctx,
            async || {
                emit(ctx, &fetch().await?);
                Ok(())
            },
            async || Ok(sections(&fetch().await?, !terms.is_empty())),
        )
        .await;
    }

    let hosts = fetch().await?;
    match ctx.output {
        OutputFormat::Json => {
            let hosts: Vec<WithState<HostListEntry>> = hosts
                .iter()
                .map(|host| WithState {
                    host,
                    state: HostState::of(host),
                })
                .collect();
            anstream::println!("{}", serde_json::to_string_pretty(&hosts)?);
        }
        OutputFormat::Jsonl => emit(ctx, &hosts),
        OutputFormat::Human => {
            let lines = sections(&hosts, !terms.is_empty());
            if let Some(mut pager) = Pager::spawn(args.no_pager) {
                let result = pager.write(&lines).await;
                return pager.close(result).await;
            }
            listing::print(&lines);
            for line in footer() {
                ctx.note(&tui::render(&line, None, true));
            }
        }
    }
    Ok(())
}

fn emit(ctx: &Ctx, hosts: &[HostListEntry]) {
    for host in hosts {
        ctx.event(Event::ListedHost {
            state: HostState::of(host),
            host,
        });
    }
    ctx.event(Event::HostPage {
        count: hosts.len(),
        next_cursor: None,
    });
}

fn matches(host: &HostListEntry, terms: &[String]) -> bool {
    let id = host.host_id.simple().to_string();
    let mut words = vec![host.name.clone(), HostState::of(host).as_str().to_string()];
    if let Some(spec) = &host.spec {
        words.extend([
            spec.site.clone(),
            spec.platform.arch.clone(),
            platform(spec),
        ]);
        words.extend(spec.location.clone());
        words.extend(
            spec.duts
                .iter()
                .flat_map(|dut| [Some(dut.board.clone()), dut.name.clone()])
                .flatten(),
        );
        words.extend(
            spec.labels
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
    }
    let text = words.join(" ").to_lowercase();
    terms.iter().all(|term| match term.strip_prefix('^') {
        Some(hex) => id.ends_with(hex),
        None => text.contains(term.as_str()),
    })
}

fn platform(spec: &HostSummary) -> String {
    let platform = &spec.platform;
    match platform.kind {
        PlatformKind::Physical => platform.model.clone().unwrap_or_default(),
        PlatformKind::Virtual => platform.hypervisor.clone().unwrap_or_default(),
    }
}

fn status(
    state: HostState,
    lease_state: HostLeaseState,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    match (state, lease_state, expires_at) {
        (HostState::Offline, _, _) => match last_seen_at {
            Some(at) => format!("seen {} ago", human_duration(now - at)),
            None => "never seen".to_string(),
        },
        (_, HostLeaseState::Idle, _) => "—".to_string(),
        (_, _, Some(at)) if at > now => format!("{} left", human_duration(at - now)),
        (_, _, Some(at)) => format!("ended {} ago", human_duration(now - at)),
        (_, _, None) => "starting".to_string(),
    }
}

fn cells(host: &HostListEntry, now: chrono::DateTime<chrono::Utc>) -> Vec<Cell> {
    let state = HostState::of(host);
    let detail = status(
        state,
        host.lease_state,
        host.current_lease_expires_at,
        host.last_seen_at,
        now,
    );
    let none = || span(DIM, "—");
    let (site, arch, platform) = match &host.spec {
        Some(spec) => (
            plain(spec.site.clone()),
            plain(spec.platform.arch.clone()),
            plain(platform(spec)),
        ),
        None => (none(), none(), none()),
    };
    let boards = boards(host);
    let boards = if boards.is_empty() {
        span(DIM, "no boards")
    } else {
        plain(boards)
    };
    vec![
        cell(plain(host.name.clone()), 0, 24),
        cell(span(state.style(), state.as_str()), 11, 11),
        cell(span(DIM, detail), 12, 14),
        cell(site, 0, 12),
        cell(arch, 0, 10),
        cell(platform, 0, 24),
        cell(boards, 0, usize::MAX),
    ]
}

fn sections(hosts: &[HostListEntry], searching: bool) -> Vec<Line> {
    let mut hosts: Vec<&HostListEntry> = hosts.iter().collect();
    hosts.sort_by_key(|host| HostState::of(host));
    let now = chrono::Utc::now();
    let rows: Vec<Vec<Cell>> = hosts.iter().map(|host| cells(host, now)).collect();
    let Some(widths) = listing::widths(&rows) else {
        let empty = if searching { "  no matches" } else { "  none" };
        return vec![vec![span(BOLD, "Hosts")], vec![span(DIM, empty)]];
    };

    let mut lines: Vec<Line> = Vec::new();
    let mut rows = hosts.iter().zip(rows).peekable();
    for (title, states) in [
        ("Available", &[HostState::Idle, HostState::Reclaimable][..]),
        ("Busy", &[HostState::Busy][..]),
        (
            "Unavailable",
            &[HostState::Maintenance, HostState::Offline][..],
        ),
    ] {
        let mut section = Vec::new();
        while let Some((_, row)) = rows.next_if(|(host, _)| states.contains(&HostState::of(host))) {
            let mut line = vec![plain("  ")];
            line.extend(listing::line(row, &widths));
            section.push(line);
        }
        if section.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Vec::new());
        }
        lines.push(vec![
            span(BOLD, title),
            span(DIM, format!(" · {}", section.len())),
        ]);
        lines.extend(section);
    }
    lines
}

fn footer() -> Vec<Line> {
    vec![
        Vec::new(),
        vec![
            plain("  "),
            span(BOLD, "Details"),
            plain(format!("  {} host show <HOST>", program())),
        ],
    ]
}

fn resolve<'a>(hosts: &'a [HostListEntry], reference: &str) -> Result<&'a HostListEntry> {
    if let Ok(id) = Uuid::parse_str(reference) {
        return hosts
            .iter()
            .find(|host| host.host_id == id)
            .with_context(|| format!("no host with id {id}"));
    }
    if let Some(host) = hosts.iter().find(|host| host.name == reference) {
        return Ok(host);
    }
    let Some(hex) = reference.strip_prefix('^') else {
        bail!("no host named {reference:?}");
    };
    let hex = hex.to_lowercase();
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{reference:?} is not a `^<hex>` short id");
    }
    let found: Vec<&HostListEntry> = hosts
        .iter()
        .filter(|host| host.host_id.simple().to_string().ends_with(&hex))
        .collect();
    match found.as_slice() {
        [host] => Ok(host),
        [] => bail!("no host matches {reference}"),
        _ => bail!(
            "{reference} matches several hosts ({}); give more digits",
            found
                .iter()
                .map(|host| host.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn show(ctx: &Ctx, client: &SwitchboardClient, reference: &str) -> Result<()> {
    let hosts = client.list_hosts().await.context("listing hosts")?;
    let entry = resolve(&hosts, reference)?;
    let host = client
        .get_host(entry.host_id)
        .await
        .with_context(|| format!("reading host {}", short(entry.host_id)))?;
    let state = HostState::new(host.live, host.maintenance, host.lease_state);

    match ctx.output {
        OutputFormat::Json => {
            let host = WithState { host: &host, state };
            anstream::println!("{}", serde_json::to_string_pretty(&host)?);
        }
        OutputFormat::Jsonl => ctx.event(Event::Host { state, host: &host }),
        OutputFormat::Human => {
            for line in display::table(&rows(&host, entry.spec.as_ref(), state), &commands(&host)) {
                anstream::println!("{}", tui::render(&line, None, true));
            }
            if let Some(spec) = &host.spec {
                anstream::println!();
                let title = format!("Spec · revision {}", host.spec_revision.unwrap_or(0));
                anstream::println!("{}", tui::render(&vec![span(BOLD, title)], None, true));
                anstream::println!("{}", serde_json::to_string_pretty(spec)?);
            }
        }
    }
    Ok(())
}

fn rows(
    host: &HostInfo,
    summary: Option<&HostSummary>,
    state: HostState,
) -> Vec<(&'static str, Line)> {
    let now = chrono::Utc::now();
    let none = || vec![span(DIM, "—")];

    let mut state_line = vec![span(state.style(), state.as_str())];
    let detail = status(
        state,
        host.lease_state,
        host.current_lease_expires_at,
        host.last_seen_at,
        now,
    );
    if detail != "—" {
        state_line.push(span(DIM, format!(" · {detail}")));
    }
    if let Some(action) = host.current_lease_expiry_action {
        state_line.push(span(
            DIM,
            match action {
                JobLeaseExpiryAction::Terminate => " · then stop",
                JobLeaseExpiryAction::Preempt => " · then reclaimable",
            },
        ));
    }

    let owner = match &host.owner {
        Some(owner) => vec![plain(
            owner.name.clone().unwrap_or_else(|| snake(&owner.kind)),
        )],
        None => none(),
    };
    let seen = host
        .last_seen_at
        .map_or_else(none, |at| vec![plain(describe_instant(at, now))]);
    let permissions = host
        .permissions
        .iter()
        .map(snake)
        .collect::<Vec<_>>()
        .join(", ");

    let mut rows = vec![
        (
            "Host",
            vec![
                plain(host.name.clone()),
                span(DIM, format!(" · {}", host.host_id)),
            ],
        ),
        ("State", state_line),
        ("Owner", owner),
        ("Seen", seen),
        ("Access", vec![plain(permissions)]),
    ];

    let Some(summary) = summary else {
        return rows;
    };

    let mut site = vec![plain(summary.site.clone())];
    if let Some(location) = &summary.location {
        site.push(span(DIM, format!(" · {location}")));
    }
    let mut platform_line = vec![
        plain(platform(summary)),
        span(
            DIM,
            format!(
                " · {} · {}",
                summary.platform.arch,
                snake(&summary.platform.kind)
            ),
        ),
    ];
    if !summary.platform.profiles.is_empty() {
        platform_line.push(span(
            DIM,
            format!(" · {}", summary.platform.profiles.join(", ")),
        ));
    }
    let resources = &summary.resources;
    rows.extend([
        ("Site", site),
        ("Platform", platform_line),
        (
            "Resources",
            vec![plain(format!(
                "{} cores · {} MB memory · {} GB storage",
                resources.cpu_cores, resources.memory_mb, resources.storage_gb
            ))],
        ),
    ]);
    if let Some(description) = &summary.description {
        rows.insert(1, ("About", vec![plain(description.clone())]));
    }

    if summary.duts.is_empty() {
        rows.push(("DUTs", none()));
    }
    for (i, dut) in summary.duts.iter().enumerate() {
        let mut line = vec![plain(dut.board.clone())];
        if let Some(name) = &dut.name {
            line.push(span(DIM, format!(" · {name}")));
        }
        rows.push((if i == 0 { "DUTs" } else { "" }, line));
    }
    if !summary.labels.is_empty() {
        let labels = summary
            .labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(("Labels", vec![plain(labels)]));
    }
    rows
}

fn commands(host: &HostInfo) -> Vec<(&'static str, Line)> {
    let tml = program();
    let predicate = format!("host.name == {:?}", host.name);
    vec![
        (
            "Run",
            vec![plain(format!(
                "{tml} job create --host {}",
                shell_quote(&predicate)
            ))],
        ),
        (
            "Jobs",
            vec![plain(format!("{tml} job list {}", shell_quote(&host.name)))],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostListEntry {
        serde_json::from_value(serde_json::json!({
            "host_id": "0679be07-6106-48aa-8057-b1d4f2e18a99",
            "name": "pton-rpi2004",
            "live": true,
            "last_seen_at": null,
            "maintenance": false,
            "lease_state": "reclaimable",
            "current_lease_expires_at": null,
            "current_lease_expiry_action": "preempt",
            "spec": {
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
                "duts": [{ "name": "Raspberry Pi Pico 2", "vendor": "Raspberry Pi", "board": "pico2" }]
            },
            "spec_revision": 3
        }))
        .unwrap()
    }

    #[test]
    fn listed_hosts_are_host_messages_ending_in_a_page() {
        let host = host();
        let listed = serde_json::to_value(Event::ListedHost {
            state: HostState::of(&host),
            host: &host,
        })
        .unwrap();
        assert_eq!(listed["type"], "host");
        assert_eq!(listed["state"], "reclaimable");
        assert_eq!(listed["host"]["name"], "pton-rpi2004");
        assert_eq!(listed["host"]["spec"]["duts"][0]["board"], "pico2");

        let page = serde_json::to_value(Event::HostPage {
            count: 1,
            next_cursor: None,
        })
        .unwrap();
        assert_eq!(
            page,
            serde_json::json!({ "type": "page", "count": 1, "next_cursor": null })
        );
    }

    #[test]
    fn terms_match_ids_names_boards_and_labels() {
        let host = host();
        let matches = |terms: &[&str]| {
            let terms: Vec<String> = terms.iter().map(|term| term.to_string()).collect();
            matches(&host, &terms)
        };
        assert!(matches(&[]));
        assert!(matches(&["^e18a99"]));
        assert!(matches(&["rpi2004", "pico2"]));
        assert!(matches(&["reclaimable", "rack=b2"]));
        assert!(matches(&["raspberry pi 5"]));
        assert!(!matches(&["^0679be07"]));
        assert!(!matches(&["pton", "nrf52840dk"]));
    }
}
