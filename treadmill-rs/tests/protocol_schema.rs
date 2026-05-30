//! Wire-schema drift guard.
//!
//! These tests serialize the JSON schema of the protocol's top-level message
//! types and compare them against committed snapshots in `protocol-schema/`.
//! Any change to the wire format (new variant, renamed field, retyped payload)
//! changes the generated schema and fails the test, forcing the snapshot --
//! and thus the protocol change -- to be reviewed explicitly.
//!
//! When a change is intentional, regenerate the snapshots with:
//!
//! ```text
//! UPDATE_SCHEMA=1 cargo test -p treadmill-rs
//! ```

use std::path::{Path, PathBuf};

use schemars::schema_for;
use treadmill_rs::api::switchboard_supervisor::{
    ServerHello, SupervisorToSwitchboard, SwitchboardToSupervisor,
};

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("protocol-schema")
}

fn check_schema(name: &str, generated: String) {
    let path = snapshot_dir().join(format!("{name}.schema.json"));

    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(snapshot_dir()).expect("create protocol-schema dir");
        std::fs::write(&path, &generated).expect("write schema snapshot");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "could not read committed schema {}: {err}.\n\
             If this is a new message type, regenerate snapshots with:\n\
             UPDATE_SCHEMA=1 cargo test -p treadmill-rs",
            path.display(),
        )
    });

    assert_eq!(
        committed,
        generated,
        "wire schema for `{name}` drifted from committed snapshot {}.\n\
         If this change is intentional, regenerate snapshots with:\n\
         UPDATE_SCHEMA=1 cargo test -p treadmill-rs",
        path.display(),
    );
}

fn pretty<T: schemars::JsonSchema>() -> String {
    let mut s = serde_json::to_string_pretty(&schema_for!(T)).expect("serialize schema");
    s.push('\n');
    s
}

#[test]
fn switchboard_to_supervisor_schema() {
    check_schema(
        "switchboard_to_supervisor",
        pretty::<SwitchboardToSupervisor>(),
    );
}

#[test]
fn supervisor_to_switchboard_schema() {
    check_schema(
        "supervisor_to_switchboard",
        pretty::<SupervisorToSwitchboard>(),
    );
}

#[test]
fn server_hello_schema() {
    check_schema("server_hello", pretty::<ServerHello>());
}
