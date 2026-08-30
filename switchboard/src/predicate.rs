//! Host-predicate evaluation.
//!
//! A job carries one CEL expression, evaluated with a host's normalized spec
//! bound as `host` to decide whether that host may run it. Image-set members
//! carry an optional refinement in the same language.
//!
//! This module is the **seam** around the expression runtime: nothing outside
//! it names a type from the underlying CEL crate. [`Engine`] is deliberately
//! narrow — compile a source string, evaluate a compiled program against one
//! spec — so the in-process implementation here can later be replaced by a
//! sandboxed one (an in-process WASM build of cel-go, or a subprocess) without
//! touching the scheduler or the routes.
//!
//! **An evaluation error means the host does not match**; it is never a job
//! failure. Callers that report to a user surface the message rather than
//! folding it into the non-matching count, because a forgotten `has()` guard
//! otherwise looks exactly like an empty fleet.

use std::fmt;

use treadmill_rs::host_spec::HostSpecV1;

/// A compiled host predicate.
pub trait Predicate: Send + Sync {
    /// Evaluate against one host spec.
    fn eval(&self, host: &HostSpecV1) -> Result<bool, EvalError>;
}

/// A runtime that compiles host predicates.
pub trait Engine: Send + Sync {
    fn compile(&self, source: &str) -> Result<Box<dyn Predicate>, CompileError>;
}

/// The source expression could not be compiled. Reported to the submitter, so
/// the message is the parser's own diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError(pub String);

/// The expression could not be evaluated against a given host: an absent key, a
/// type mismatch, or a result that was not boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError(pub String);

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CompileError {}
impl std::error::Error for EvalError {}

/// The in-process [`Engine`], backed by the `cel` crate.
///
/// TODO: this runtime is not hardened against hostile input. The parser
/// exposes no depth, node-count or cost limit, and recurses per nesting level;
/// deep enough nesting overflows the stack, which aborts the process rather
/// than unwinding, so it cannot be caught here. Measured abort depths against
/// `cel` 0.14 on an 8 MiB stack, debug: ~52 nested parentheses, ~47 nested
/// brackets, ~212 for `+` / field / index chains — all reachable well inside a
/// single request body, and lower again on a 2 MiB tokio worker stack. Until
/// the source limits of the plan's §5.1 land (a byte cap plus a pre-parse
/// nesting and token scan), or evaluation moves behind the isolation boundary
/// this trait exists to allow, predicate submission must be treated as trusted.
#[derive(Debug, Default, Clone, Copy)]
pub struct CelEngine;

impl Engine for CelEngine {
    fn compile(&self, source: &str) -> Result<Box<dyn Predicate>, CompileError> {
        let program = cel::Program::compile(source).map_err(|e| CompileError(e.to_string()))?;
        Ok(Box::new(CelPredicate(program)))
    }
}

struct CelPredicate(cel::Program);

impl Predicate for CelPredicate {
    fn eval(&self, host: &HostSpecV1) -> Result<bool, EvalError> {
        let mut context = cel::Context::default();
        context
            .add_variable("host", host)
            .map_err(|e| EvalError(format!("binding host spec: {e}")))?;
        match self.0.execute(&context) {
            Ok(cel::Value::Bool(matched)) => Ok(matched),
            Ok(other) => Err(EvalError(format!(
                "expression is not a predicate: it evaluated to {}",
                type_name(&other)
            ))),
            Err(e) => Err(EvalError(e.to_string())),
        }
    }
}

fn type_name(value: &cel::Value) -> &'static str {
    use cel::Value;
    match value {
        Value::List(_) => "a list",
        Value::Map(_) => "a map",
        Value::Function(..) => "a function",
        Value::Int(_) | Value::UInt(_) => "an integer",
        Value::Float(_) => "a float",
        Value::String(_) => "a string",
        Value::Bytes(_) => "bytes",
        Value::Bool(_) => "a bool",
        Value::Duration(_) => "a duration",
        Value::Timestamp(_) => "a timestamp",
        Value::Null => "null",
        _ => "an unsupported type",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use treadmill_rs::host_spec::{
        Console, DebugAccess, DebugProbe, Dut, HostSpecV1, Platform, Resources, SpecVersionV1,
    };
    use uuid::Uuid;

    use super::*;

    fn host() -> HostSpecV1 {
        HostSpecV1 {
            spec_version: SpecVersionV1::V1,
            id: Uuid::nil(),
            name: "cam-rpi4-01".into(),
            description: None,
            site: "cambridge".into(),
            location: Some("rack4/shelf2".into()),
            platform: Platform::Physical {
                arch: "aarch64".into(),
                profiles: vec!["rpi4-uboot-sd".into()],
                vendor: "Raspberry Pi Ltd".into(),
                model: "Raspberry Pi 4 Model B Rev 1.5".into(),
            },
            resources: Resources {
                cpu_cores: 4,
                memory_mb: 8192,
                storage_gb: 64,
            },
            labels: BTreeMap::from([("bench".to_string(), "nordic-bringup".to_string())]),
            duts: vec![
                Dut {
                    name: Some("nRF52840-DK #1".into()),
                    serial: Some("1050123456".into()),
                    vendor: "Nordic Semiconductor".into(),
                    board: "nrf52840dk".into(),
                    arch: vec!["cortex-m4".into()],
                    connectivity: vec!["ble".into(), "ieee802154".into(), "usb".into()],
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
                },
                Dut {
                    name: Some("STM32F4 Discovery".into()),
                    serial: None,
                    vendor: "STMicroelectronics".into(),
                    board: "stm32f4discovery".into(),
                    arch: vec!["cortex-m4".into()],
                    connectivity: vec!["usb".into()],
                    debug: None,
                    console: None,
                    labels: BTreeMap::new(),
                },
            ],
        }
    }

    fn eval(source: &str) -> Result<bool, EvalError> {
        CelEngine.compile(source).expect("compiles").eval(&host())
    }

    fn matches(source: &str) -> bool {
        eval(source).expect("evaluates")
    }

    #[test]
    fn default_predicate_matches_everything() {
        assert!(matches("true"));
    }

    #[test]
    fn scalar_and_nested_fields() {
        assert!(matches(
            "host.site == 'cambridge' && host.resources.memory_mb >= 4096"
        ));
        assert!(!matches("host.resources.cpu_cores > 8"));
    }

    #[test]
    fn dut_existence_and_cardinality() {
        assert!(matches(
            "host.duts.exists(d, d.vendor.contains('Nordic') && d.board == 'nrf52840dk')"
        ));
        assert!(matches("host.duts.size() == 2"));
        assert!(matches(
            "host.duts.filter(d, d.arch.exists(a, a == 'cortex-m4')).size() >= 2"
        ));
        assert!(matches("host.duts.all(d, !('quarantined' in d.labels))"));
    }

    #[test]
    fn variant_fields_need_a_has_guard() {
        assert!(matches(
            "host.platform.kind == 'physical' && has(host.platform.model) \
             && host.platform.model.contains('Raspberry Pi 4')"
        ));
        // Rule 2: the absent variant's field is missing, not null.
        assert!(!matches("has(host.platform.hypervisor)"));
    }

    #[test]
    fn optional_blocks_and_labels() {
        assert!(matches(
            "host.duts.exists(d, has(d.debug) && d.debug.protocol == 'swd' \
             && d.debug.probe.vendor == 'SEGGER')"
        ));
        assert!(matches(
            "'bench' in host.labels && host.labels['bench'] == 'nordic-bringup'"
        ));
        assert!(matches("'ble' in host.duts[0].connectivity"));
        assert!(matches(
            "'rpi4-uboot-sd' in host.platform.profiles && host.description == null"
        ));
    }

    /// A misspelled field errors rather than quietly reading as null, which is
    /// what lets the enqueue-time diagnostic tell a typo from an empty fleet.
    #[test]
    fn unknown_field_access_is_an_error() {
        let err = eval("host.duts.exists(d, d.borad == 'nrf52840dk')").unwrap_err();
        assert!(err.to_string().contains("borad"), "{err}");
    }

    /// Rule 3: indexing an absent label key errors, so predicates guard with `in`.
    #[test]
    fn unguarded_label_index_is_an_error() {
        assert!(eval("host.labels['absent'] == 'x'").is_err());
    }

    #[test]
    fn non_boolean_result_is_an_error() {
        let err = eval("host.duts.size()").unwrap_err();
        assert!(err.to_string().contains("not a predicate"), "{err}");
    }

    #[test]
    fn syntax_errors_are_reported_at_compile_time() {
        assert!(CelEngine.compile("host.site ==").is_err());
    }
}
