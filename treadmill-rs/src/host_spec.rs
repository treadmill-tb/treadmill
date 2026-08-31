//! The host spec: an admin-authored, versioned description of what a host *is*.
//!
//! One document per host describes its site, chassis, bootable machine
//! profiles, and attached devices under test. The switchboard stores it
//! verbatim, serves it to clients and to the host's own supervisor, and binds
//! it as `host` when evaluating a job's CEL predicate. It carries description
//! only: ownership, liveness, maintenance and job assignment are columns on the
//! `hosts` row, not fields here.
//!
//! **No secrets belong in a spec.** Any subject holding `read` on a host sees
//! the whole document.
//!
//! Three serialization rules keep predicates free of defensive guards:
//!
//! 1. Optional non-variant fields are always serialized, `null` when absent, so
//!    a predicate touching one needs no `has()` guard.
//! 2. Variant-specific fields are genuinely absent on other variants
//!    ([`Platform::Virtual`] has no `model`), so reaching into one does need a
//!    `has()` guard.
//! 3. Label maps are ordered and may be empty; CEL indexing on an absent key
//!    errors, so predicates guard with `in`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A host spec at whatever version it was written under.
///
/// Untagged because each version type carries its own `spec_version` field:
/// that keeps the discriminant inside the document being validated, so a
/// rejection names the offending path (`duts[2].debug.probe.serail`) instead of
/// the document root, which an internally-tagged enum cannot do.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HostSpec {
    V1(HostSpecV1),
}

impl HostSpec {
    /// The version this document was written under, without unpacking it.
    pub fn version(&self) -> &'static str {
        match self {
            HostSpec::V1(_) => "v1",
        }
    }

    /// Fold the document forward to the current version.
    ///
    /// Every read goes through here, so the evaluator, the console and the
    /// supervisor only ever see the latest version. Adding a version means
    /// adding one step from its immediate predecessor; at v1 the chain is empty
    /// and this is the identity.
    pub fn into_latest(self) -> HostSpecV1 {
        match self {
            HostSpec::V1(v1) => v1,
        }
    }
}

/// The `spec_version` discriminant of a [`HostSpecV1`] document.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecVersionV1 {
    #[serde(rename = "v1")]
    V1,
}

/// Version 1 of the host spec.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSpecV1 {
    pub spec_version: SpecVersionV1,
    /// Must equal the `host_id` of the host this document describes.
    pub id: Uuid,
    /// Display handle, e.g. `cam-rpi4-01`. Deliberately not unique; nothing
    /// routes on it.
    pub name: String,
    /// What this host is, in prose.
    pub description: Option<String>,
    /// The site the host lives at, e.g. `cambridge`. Flat, so a predicate reads
    /// `host.site == 'cambridge'`.
    pub site: String,
    /// Where in the site, e.g. `rack4/shelf2`. Free text.
    pub location: Option<String>,
    pub platform: Platform,
    pub resources: Resources,
    /// Operator-defined labels. CEL map indexing errors on an absent key, so
    /// predicates guard with `'key' in host.labels`.
    pub labels: BTreeMap<String, String>,
    /// The devices under test wired to this host, in the order the operator
    /// listed them. May be empty.
    pub duts: Vec<Dut>,
}

/// The machine a host is, and the images it can boot.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Platform {
    Physical {
        /// The host's own CPU architecture, e.g. `aarch64`, `x86_64`.
        ///
        /// Redundant — a profile implies it — but kept so "any aarch64 host"
        /// does not require enumerating profiles. Nothing enforces consistency
        /// between the two.
        arch: String,
        /// The whole machine configurations this host can boot, e.g.
        /// `rpi4-uboot-sd`, `q35-virtio-uefi`, `q35-virtio-bios`,
        /// `netboot-nbd`. A profile names a complete configuration the way a
        /// target triple does; an image set member matches one by equality.
        ///
        /// An array because one host may genuinely serve several. Governed by
        /// convention, not a registry.
        profiles: Vec<String>,
        vendor: String,
        model: String,
    },
    Virtual {
        /// The architecture the guest is presented with.
        arch: String,
        /// As [`Platform::Physical::profiles`].
        profiles: Vec<String>,
        /// e.g. `qemu`.
        hypervisor: String,
    },
}

impl Platform {
    /// The machine configurations this host can boot, whichever variant it is.
    /// An image set member matches one of these by equality.
    pub fn profiles(&self) -> &[String] {
        match self {
            Platform::Physical { profiles, .. } | Platform::Virtual { profiles, .. } => profiles,
        }
    }

    /// The variant discriminant, as a predicate spells it.
    pub fn kind(&self) -> PlatformKind {
        match self {
            Platform::Physical { .. } => PlatformKind::Physical,
            Platform::Virtual { .. } => PlatformKind::Virtual,
        }
    }

    /// The architecture, whichever variant it is.
    pub fn arch(&self) -> &str {
        match self {
            Platform::Physical { arch, .. } | Platform::Virtual { arch, .. } => arch,
        }
    }
}

/// Which [`Platform`] variant a host is, without its variant-specific fields.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformKind {
    Physical,
    Virtual,
}

/// The ceiling available to a single job on this host.
///
/// Unsigned, so CEL sees these as `uint`: comparisons against a plain literal
/// work (`host.resources.memory_mb >= 4096`), but arithmetic needs an unsigned
/// literal (`host.resources.memory_mb / 1024u >= 8`).
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpu_cores: u32,
    pub memory_mb: u32,
    pub storage_gb: u32,
}

/// One device under test wired to a host.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dut {
    /// Display label, e.g. `nRF52840-DK #1`.
    pub name: Option<String>,
    /// The board's **own** serial number, not its debug probe's.
    pub serial: Option<String>,
    pub vendor: String,
    /// The board this is, e.g. `nrf52840dk`.
    pub board: String,
    /// The architectures of the board's cores, e.g. `cortex-m4`. An array
    /// because a heterogeneous-core part has more than one. May be empty.
    pub arch: Vec<String>,
    /// What the board can talk over, e.g. `ble`, `ieee802154`, `usb`, `wifi`,
    /// `ethernet`, `can`. Governed by convention, not a registry. May be empty.
    pub connectivity: Vec<String>,
    pub debug: Option<DebugAccess>,
    pub console: Option<Console>,
    /// As [`HostSpecV1::labels`], scoped to this DUT.
    pub labels: BTreeMap<String, String>,
}

/// How the board is programmed and debugged.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugAccess {
    /// The wire protocol, e.g. `swd`, `jtag`. Governed by convention, not a
    /// registry.
    pub protocol: String,
    pub probe: DebugProbe,
}

/// The debug probe attached to a board.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugProbe {
    /// e.g. `SEGGER`, `STMicroelectronics`.
    pub vendor: String,
    /// e.g. `J-Link OB`, `ST-LINK/V2-1`.
    pub model: String,
    /// The probe's own serial, which is how a host-side tool addresses it.
    pub serial: Option<String>,
}

/// The board's console, as the host sees it.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Console {
    Uart {
        /// The host-side device node. Prefer a stable `/dev/serial/by-id/...`
        /// path over a `/dev/ttyACM*` name, which is enumeration-order
        /// dependent.
        device: String,
        baud: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1(duts: Vec<Dut>) -> HostSpecV1 {
        HostSpecV1 {
            spec_version: SpecVersionV1::V1,
            id: Uuid::nil(),
            name: "cam-qemu-04".into(),
            description: None,
            site: "cambridge".into(),
            location: None,
            platform: Platform::Virtual {
                arch: "x86_64".into(),
                profiles: vec!["q35-virtio-uefi".into()],
                hypervisor: "qemu".into(),
            },
            resources: Resources {
                cpu_cores: 8,
                memory_mb: 16384,
                storage_gb: 200,
            },
            labels: BTreeMap::new(),
            duts,
        }
    }

    /// Rule 1: an absent optional is `null`, not an omitted key.
    #[test]
    fn optional_fields_serialize_as_null() {
        let json = serde_json::to_value(HostSpec::V1(v1(vec![]))).unwrap();
        assert_eq!(json["description"], serde_json::Value::Null);
        assert_eq!(json["location"], serde_json::Value::Null);
        assert_eq!(json["spec_version"], "v1");
    }

    /// Rule 2: a variant's fields do not appear on the other variant.
    #[test]
    fn variant_fields_are_absent_not_null() {
        let json = serde_json::to_value(v1(vec![])).unwrap();
        assert_eq!(json["platform"]["kind"], "virtual");
        assert_eq!(json["platform"]["hypervisor"], "qemu");
        assert!(json["platform"].get("model").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let spec = HostSpec::V1(v1(vec![Dut {
            name: Some("nRF52840-DK #1".into()),
            serial: Some("1050123456".into()),
            vendor: "Nordic Semiconductor".into(),
            board: "nrf52840dk".into(),
            arch: vec!["cortex-m4".into()],
            connectivity: vec!["ble".into(), "usb".into()],
            debug: Some(DebugAccess {
                protocol: "swd".into(),
                probe: DebugProbe {
                    vendor: "SEGGER".into(),
                    model: "J-Link OB".into(),
                    serial: Some("000683012345".into()),
                },
            }),
            console: Some(Console::Uart {
                device: "/dev/ttyACM0".into(),
                baud: 115200,
            }),
            labels: BTreeMap::from([("radio".to_string(), "ble".to_string())]),
        }]));
        let encoded = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<HostSpec>(&encoded).unwrap(), spec);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut json = serde_json::to_value(v1(vec![])).unwrap();
        json["colour"] = "beige".into();
        assert!(serde_json::from_value::<HostSpecV1>(json).is_err());
    }

    /// The seam every read goes through; the identity while the chain is empty.
    #[test]
    fn normalizing_yields_the_latest_version() {
        let spec = HostSpec::V1(v1(vec![]));
        assert_eq!(spec.version(), "v1");
        assert_eq!(spec.into_latest(), v1(vec![]));
    }
}
