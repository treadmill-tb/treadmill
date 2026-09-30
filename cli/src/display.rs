use treadmill_rs::api::switchboard::hosts::{HostLeaseState, HostListEntry};
use uuid::Uuid;

/// Short display form of a UUID: `^` plus its final eight hexadecimal digits.
pub fn short(id: Uuid) -> String {
    let text = id.simple().to_string();
    format!("^{}", &text[text.len() - 8..])
}

pub fn snake(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default()
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
        match (host.live, host.maintenance, host.lease_state) {
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
