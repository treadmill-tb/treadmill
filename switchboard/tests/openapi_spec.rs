//! OpenAPI schema drift guard.
//!
//! This test generates the OpenAPI specification for the Switchboard API
//! and compares it against a committed snapshot in `api-spec/openapi.yaml`.
//!
//! When a change is intentional, regenerate the snapshot with:
//!
//! ```text
//! UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard --test openapi_spec
//! ```

use std::path::{Path, PathBuf};
use treadmill_switchboard::routes::openapi_spec;

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("api-spec")
}

#[test]
fn test_openapi_schema_drift() {
    let api = openapi_spec();

    let generated = serde_norway::to_string(&api).expect("serialize openapi spec");

    let path = snapshot_dir().join("openapi.yaml");

    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(snapshot_dir()).expect("create api-spec dir");
        std::fs::write(&path, &generated).expect("write openapi snapshot");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "could not read committed openapi spec {}: {err}.\n\
             Regenerate the snapshot with:\n\
             UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard",
            path.display(),
        )
    });

    if committed != generated {
        // Use a simple assertion for the error message, but the diff in git will be more useful.
        assert_eq!(
            committed,
            generated,
            "OpenAPI spec drifted from committed snapshot {}.\n\
             If this change is intentional, regenerate the snapshot with:\n\
             UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard",
            path.display(),
        );
    }
}
