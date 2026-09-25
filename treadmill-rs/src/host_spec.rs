//! The host spec: an admin-authored, versioned description of what a host *is*.
//!
//! One document per host describes its site, chassis, bootable machine
//! profiles, attached devices under test and how they are wired. The
//! switchboard stores it verbatim, serves it to clients and to jobs (as
//! `host-spec.json`), and binds it as `host` when evaluating a job's CEL
//! predicate. It carries description only: ownership, liveness, maintenance and
//! job assignment are columns on the `hosts` row, not fields here.
//!
//! **No secrets belong in a spec.** Any subject holding `read` on a host sees
//! the whole document.
//!
//! # Versions
//!
//! Stored documents are upgraded to the latest version on read; writes must
//! carry the latest version. Consumers other than the switchboard should ignore
//! unknown fields, so that adding one is cheap, and may rely on `spec_version`
//! to tell breaking changes apart.
//!
//! # Serialization rules
//!
//! Three rules keep predicates free of defensive guards:
//!
//! 1. Optional non-variant fields are always serialized, `null` when absent, so
//!    a predicate touching one needs no `has()` guard.
//! 2. Variant-specific fields are genuinely absent on other variants
//!    ([`Platform::Virtual`] has no `model`), so reaching into one does need a
//!    `has()` guard.
//! 3. Maps (labels, GPIO controllers and pins) are ordered and may be empty;
//!    CEL indexing on an absent key errors, so predicates guard with `in`.
//!
//! # Conventions
//!
//! - A DUT's `board` is a lowercase identifier matching `^[a-z0-9][a-z0-9_-]*$`,
//!   e.g. `nrf52840dk`; the display name goes in `name`.
//! - GPIO pins are keyed by the **DUT-side** pin name, e.g. `P0.13`. Their
//!   `modes` are seen from the host: `digital_in` (the host reads the pin),
//!   `digital_out` (the host drives it).
//! - GPIO controllers are declared once per host, since one controller may
//!   serve several DUTs. Everything driver-specific lives under `config`, on
//!   both controllers and pins, so common fields added later cannot collide
//!   with driver fields.
//!
//! ## The `linux-gpiochip` driver
//!
//! A GPIO chip driven through the Linux GPIO character device. The controller's
//! `config.label` is the chip's label (as `gpioinfo` prints it, e.g.
//! `pinctrl-rp1`), optionally narrowed by `config.usb_serial` or
//! `config.usb_port` to tell identical expanders apart. A pin's `config.offset`
//! is its line offset within that chip. Neither `/dev/gpiochipN` paths nor
//! sysfs GPIO numbers belong here: both depend on probe order and kernel
//! version.

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
    V2(HostSpecV2),
}

/// The version every stored document is upgraded to, and the only one
/// accepted on write.
pub type HostSpecLatest = HostSpecV2;

impl HostSpec {
    /// The version this document was written under, without unpacking it.
    pub fn version(&self) -> &'static str {
        match self {
            HostSpec::V1(_) => "v1",
            HostSpec::V2(_) => "v2",
        }
    }

    /// Fold the document forward to the current version.
    ///
    /// Every read goes through here, so the evaluator, the console and jobs
    /// only ever see the latest version. Adding a version means adding one
    /// step from its immediate predecessor.
    pub fn into_latest(self) -> HostSpecLatest {
        match self {
            HostSpec::V1(v1) => v1.into(),
            HostSpec::V2(v2) => v2,
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
    pub duts: Vec<DutV1>,
}

/// The `spec_version` discriminant of a [`HostSpecV2`] document.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecVersionV2 {
    #[serde(rename = "v2")]
    V2,
}

/// Version 2 of the host spec.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSpecV2 {
    pub spec_version: SpecVersionV2,
    /// Must equal the `host_id` of the host this document describes.
    pub id: Uuid,
    /// Display handle, e.g. `cam-rpi5-01`. Deliberately not unique; nothing
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
    /// The host's GPIO controllers, keyed by a name the DUTs' pins refer to,
    /// e.g. `rp1`. May be empty.
    pub gpio_controllers: BTreeMap<String, GpioController>,
    /// The devices under test wired to this host, in the order the operator
    /// listed them. May be empty.
    pub duts: Vec<DutV2>,
}

impl From<HostSpecV1> for HostSpecV2 {
    fn from(v1: HostSpecV1) -> Self {
        HostSpecV2 {
            spec_version: SpecVersionV2::V2,
            id: v1.id,
            name: v1.name,
            description: v1.description,
            site: v1.site,
            location: v1.location,
            platform: v1.platform,
            resources: v1.resources,
            labels: v1.labels,
            gpio_controllers: BTreeMap::new(),
            duts: v1.duts.into_iter().map(DutV2::from).collect(),
        }
    }
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
pub struct DutV1 {
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

/// One device under test wired to a host.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DutV2 {
    /// Display name, e.g. `nRF52840-DK`.
    pub name: Option<String>,
    /// The board's **own** serial number, not its debug probe's.
    pub serial: Option<String>,
    pub vendor: String,
    /// The board this is, as a lowercase identifier, e.g. `nrf52840dk`.
    pub board: String,
    /// The architectures of the board's cores, e.g. `cortex-m4`. An array
    /// because a heterogeneous-core part has more than one. May be empty.
    pub arch: Vec<String>,
    /// What the board can talk over, e.g. `ble`, `ieee802154`, `usb`, `wifi`,
    /// `ethernet`, `can`. Governed by convention, not a registry. May be empty.
    pub connectivity: Vec<String>,
    pub debug: Option<DebugAccess>,
    pub console: Option<Console>,
    /// The board's pins wired to a GPIO controller, keyed by the board's own
    /// pin name, e.g. `P0.13`. May be empty.
    pub gpio: BTreeMap<String, GpioPin>,
    /// As [`HostSpecV2::labels`], scoped to this DUT.
    pub labels: BTreeMap<String, String>,
}

impl From<DutV1> for DutV2 {
    fn from(v1: DutV1) -> Self {
        DutV2 {
            name: v1.name,
            serial: v1.serial,
            vendor: v1.vendor,
            board: v1.board,
            arch: v1.arch,
            connectivity: v1.connectivity,
            debug: v1.debug,
            console: v1.console,
            gpio: BTreeMap::new(),
            labels: v1.labels,
        }
    }
}

/// A GPIO controller on the host, e.g. a SoC's pin controller or a USB GPIO
/// expander.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpioController {
    /// How the host drives this controller, e.g. `linux-gpiochip`. Governed by
    /// convention, not a registry.
    pub driver: String,
    /// Driver-specific settings that locate the controller.
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// One DUT pin wired to a host GPIO controller.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpioPin {
    /// What the pin is for on the board, e.g. `LED1`, `BUTTON1`.
    pub label: Option<String>,
    /// How the host may use the pin: `digital_in`, `digital_out`. Governed by
    /// convention, not a registry.
    pub modes: Vec<String>,
    /// The key of the controller in `gpio_controllers`.
    pub controller: String,
    /// Driver-specific settings that locate the pin on its controller.
    pub config: serde_json::Map<String, serde_json::Value>,
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
    use serde_json::json;

    use super::*;

    fn v1() -> HostSpecV1 {
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
            duts: vec![DutV1 {
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
            }],
        }
    }

    fn v2() -> HostSpecV2 {
        let mut spec = HostSpecV2::from(v1());
        spec.gpio_controllers = BTreeMap::from([(
            "rp1".to_string(),
            GpioController {
                driver: "linux-gpiochip".into(),
                config: json!({ "label": "pinctrl-rp1" })
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        )]);
        spec.duts[0].gpio = BTreeMap::from([(
            "P0.13".to_string(),
            GpioPin {
                label: Some("LED1".into()),
                modes: vec!["digital_in".into()],
                controller: "rp1".into(),
                config: json!({ "offset": 20 }).as_object().unwrap().clone(),
            },
        )]);
        spec
    }

    #[test]
    fn optional_fields_serialize_as_null() {
        let json = serde_json::to_value(v2()).unwrap();
        assert_eq!(json["description"], serde_json::Value::Null);
        assert_eq!(json["location"], serde_json::Value::Null);
        assert_eq!(json["spec_version"], "v2");
    }

    #[test]
    fn variant_fields_are_absent_not_null() {
        let json = serde_json::to_value(v2()).unwrap();
        assert_eq!(json["platform"]["kind"], "virtual");
        assert_eq!(json["platform"]["hypervisor"], "qemu");
        assert!(json["platform"].get("model").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        for spec in [HostSpec::V1(v1()), HostSpec::V2(v2())] {
            let encoded = serde_json::to_string(&spec).unwrap();
            assert_eq!(serde_json::from_str::<HostSpec>(&encoded).unwrap(), spec);
        }
    }

    #[test]
    fn v1_upgrades_with_empty_gpio() {
        let spec = HostSpec::V1(v1());
        assert_eq!(spec.version(), "v1");
        let latest = spec.into_latest();
        assert_eq!(latest.spec_version, SpecVersionV2::V2);
        assert!(latest.gpio_controllers.is_empty());
        assert!(latest.duts[0].gpio.is_empty());
        assert_eq!(latest.duts[0].board, "nrf52840dk");
    }

    #[test]
    fn v2_is_already_latest() {
        let spec = HostSpec::V2(v2());
        assert_eq!(spec.version(), "v2");
        assert_eq!(spec.into_latest(), v2());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut json = serde_json::to_value(v1()).unwrap();
        json["gpio_controllers"] = json!({});
        assert!(serde_json::from_value::<HostSpecV1>(json).is_err());

        let base = serde_json::to_value(v2()).unwrap();
        for pointer in ["", "/gpio_controllers/rp1", "/duts/0", "/duts/0/gpio/P0.13"] {
            let mut json = base.clone();
            json.pointer_mut(pointer).unwrap()["colour"] = "beige".into();
            assert!(
                serde_json::from_value::<HostSpecV2>(json).is_err(),
                "accepted an unknown field at `{pointer}`"
            );
        }
    }

    #[test]
    fn config_accepts_arbitrary_json() {
        let config = json!({ "usb_serial": "A1", "nested": { "list": [1, null, true] } });
        let mut json = serde_json::to_value(v2()).unwrap();
        json["gpio_controllers"]["rp1"]["config"] = config.clone();
        json["duts"][0]["gpio"]["P0.13"]["config"] = config.clone();
        let spec: HostSpecV2 = serde_json::from_value(json).unwrap();
        assert_eq!(
            serde_json::Value::from(spec.gpio_controllers["rp1"].config.clone()),
            config
        );
        assert_eq!(
            serde_json::Value::from(spec.duts[0].gpio["P0.13"].config.clone()),
            config
        );
    }
}
