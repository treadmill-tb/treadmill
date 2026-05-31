//! OpenAPI schema drift guard.
//!
//! This test generates the OpenAPI specification for the Switchboard API
//! and compares it against a committed snapshot in `api-spec/openapi.json`.
//!
//! When a change is intentional, regenerate the snapshot with:
//!
//! ```text
//! UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard
//! ```

use aide::openapi::{Info, OpenApi};
use std::path::{Path, PathBuf};
use treadmill_switchboard::routes::api_router;

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("api-spec")
}

#[test]
fn test_openapi_schema_drift() {
    let mut api = OpenApi {
        info: Info {
            title: "Treadmill Switchboard API".to_string(),
            version: "0.1.0".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = api_router().finish_api(&mut api);

    let generated = serde_json::to_string_pretty(&api).expect("serialize openapi spec") + "\n";

    let path = snapshot_dir().join("openapi.json");

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
