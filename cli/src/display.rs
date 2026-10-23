use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::hosts::{HostLeaseState, HostListEntry};
use treadmill_rs::api::switchboard::images::ImageSetInfo;
use treadmill_rs::api::switchboard::jobs::{JobImageReference, JobInfo, JobLeaseExpiryAction};
use uuid::Uuid;

use crate::tui::{BOLD, DIM, ERR, Line, OK, WARN, plain, span};

pub const SSHWS_PROTOCOL: &str = "sshws";
const SYSTEM_SUBJECT_ID: Uuid = Uuid::from_u128(2);

/// Short display form of a UUID: `^` plus its final eight hexadecimal digits.
pub fn short(id: Uuid) -> String {
    let text = id.simple().to_string();
    format!("^{}", &text[text.len() - 8..])
}

pub fn program() -> String {
    let arg0 = std::env::args_os().next().map_or_else(
        || "tml".to_string(),
        |arg| arg.to_string_lossy().into_owned(),
    );
    shell_quote(&arg0)
}

pub fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:=@^+,%".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// A remaining duration, coarsely: `45s`, `12m`, `3h05m`, `2d04h`.
pub fn human_duration(d: chrono::TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d{:02}h", secs / 86400, secs % 86400 / 3600),
    }
}

/// `at` in local time, with how far it is from `now`.
pub fn describe_instant(
    at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let local = at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S %:z");
    if at > now {
        format!("{local} ({} left)", human_duration(at - now))
    } else {
        format!("{local} ({} ago)", human_duration(now - at))
    }
}

pub fn snake(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default()
}

pub fn ready(job: &JobInfo) -> bool {
    job.state == JobState::Ready
        && job
            .services
            .iter()
            .any(|service| service.protocol == SSHWS_PROTOCOL)
}

pub fn phase(job: &JobInfo) -> String {
    let state = snake(&job.state);
    match (job.state, job.initializing_stage) {
        (JobState::Initializing, Some(stage)) => format!("{state} · {}", snake(&stage)),
        (JobState::Ready, _) if !ready(job) => format!("{state} · waiting for SSH"),
        _ => state,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Idle,
    Reclaimable,
    Busy,
    Maintenance,
    Offline,
}

impl HostState {
    pub fn of(host: &HostListEntry) -> Self {
        Self::new(host.live, host.maintenance, host.lease_state)
    }

    pub fn new(live: bool, maintenance: bool, lease_state: HostLeaseState) -> Self {
        match (live, maintenance, lease_state) {
            (false, _, _) => Self::Offline,
            (true, true, _) => Self::Maintenance,
            (true, false, HostLeaseState::Busy) => Self::Busy,
            (true, false, HostLeaseState::Reclaimable) => Self::Reclaimable,
            (true, false, HostLeaseState::Idle) => Self::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Reclaimable => "reclaimable",
            Self::Busy => "busy",
            Self::Maintenance => "maintenance",
            Self::Offline => "offline",
        }
    }

    pub fn style(self) -> anstyle::Style {
        match self {
            Self::Idle | Self::Reclaimable => OK,
            Self::Offline => ERR,
            Self::Busy | Self::Maintenance => WARN,
        }
    }
}

pub fn is_system(set: &ImageSetInfo) -> bool {
    set.owner
        .as_ref()
        .is_some_and(|owner| owner.id == SYSTEM_SUBJECT_ID)
}

pub fn image_name(set: &ImageSetInfo) -> String {
    match set.owner.as_ref().and_then(|owner| owner.name.as_deref()) {
        Some(owner) if !is_system(set) => format!("{owner}/{}", set.display_name),
        _ => set.display_name.clone(),
    }
}

pub fn boards(host: &HostListEntry) -> String {
    host.spec.as_ref().map_or(String::new(), |spec| {
        spec.duts
            .iter()
            .map(|dut| dut.board.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    })
}

pub async fn job_rows(client: &SwitchboardClient, job: &JobInfo) -> Vec<(&'static str, Line)> {
    let (hosts, sets) = tokio::join!(client.list_hosts(), client.list_image_sets());
    let (hosts, sets) = (hosts.unwrap_or_default(), sets.unwrap_or_default());
    let now = chrono::Utc::now();
    let none = || vec![span(DIM, "—")];

    let name = match &job.label {
        Some(label) => vec![
            plain(job.job_id.to_string()),
            span(DIM, " · "),
            plain(label.clone()),
        ],
        None => vec![plain(job.job_id.to_string())],
    };
    let state = match (job.state, job.termination_reason) {
        (JobState::Finalized, Some(reason)) => vec![
            plain(snake(&job.state)),
            span(DIM, format!(" · {}", snake(&reason))),
        ],
        _ => vec![plain(phase(job))],
    };
    let image = match &job.image.reference {
        JobImageReference::ImageSet { set_id, generation } => {
            let name = sets
                .iter()
                .find(|set| set.id == *set_id)
                .map_or_else(|| short(*set_id), image_name);
            vec![plain(name), span(DIM, format!(" v{generation}"))]
        }
        JobImageReference::Image { manifest_digest } => vec![plain(manifest_digest.to_string())],
    };
    let host = match job.dispatched_on_host_id {
        None => none(),
        Some(host_id) => match hosts.iter().find(|host| host.host_id == host_id) {
            None => vec![plain(short(host_id))],
            Some(host) => with_boards(host),
        },
    };
    let lease = match job.state {
        JobState::Terminating | JobState::Finalized => none(),
        _ => lease(job, now),
    };
    let address = job
        .job_ip_address
        .map_or_else(none, |address| vec![plain(address.to_string())]);
    let mut services = Vec::new();
    for (i, service) in job.services.iter().enumerate() {
        if i > 0 {
            services.push(plain(", "));
        }
        services.push(plain(service.name.clone()));
        services.push(span(DIM, format!(" ({})", service.protocol)));
    }
    if services.is_empty() {
        services = none();
    }

    vec![
        ("Job", name),
        ("State", state),
        ("Image", image),
        ("Host", host),
        ("Lease", lease),
        ("Address", address),
        ("Services", services),
    ]
}

/// A job's lease: when it ends, or how long it runs from the job's start, and
/// what happens then.
pub fn lease(job: &JobInfo, now: chrono::DateTime<chrono::Utc>) -> Line {
    let then = match job.lease_expiry_action {
        JobLeaseExpiryAction::Terminate => " · then stop",
        JobLeaseExpiryAction::Preempt => " · then reclaimable",
    };
    let when = match job.lease_expires_at {
        Some(at) if at > now => format!("ends {}", describe_instant(at, now)),
        Some(at) => format!("ended {}", describe_instant(at, now)),
        None => format!(
            "{} from start",
            human_duration(chrono::TimeDelta::seconds(job.lease_duration_secs))
        ),
    };
    vec![plain(when), span(DIM, then)]
}

pub fn with_boards(host: &HostListEntry) -> Line {
    let mut line = vec![plain(host.name.clone())];
    let boards = boards(host);
    if !boards.is_empty() {
        line.push(span(DIM, format!(" · {boards}")));
    }
    line
}

pub fn table(rows: &[(&str, Line)], commands: &[(&str, Line)]) -> Vec<Line> {
    let width = rows
        .iter()
        .chain(commands)
        .map(|(title, _)| title.len())
        .max()
        .unwrap_or(0);
    let line = |(title, value): &(&str, Line)| {
        let mut line = vec![
            plain("  "),
            span(BOLD, format!("{title:<width$}")),
            plain("  "),
        ];
        line.extend(value.iter().cloned());
        line
    };
    let mut lines: Vec<Line> = rows.iter().map(line).collect();
    if !commands.is_empty() {
        lines.push(Vec::new());
        lines.extend(commands.iter().map(line));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_are_quoted_only_when_needed() {
        assert_eq!(
            shell_quote("treadmill/debian-trixie"),
            "treadmill/debian-trixie"
        );
        assert_eq!(
            shell_quote("host.site == \"lab\""),
            "'host.site == \"lab\"'"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
