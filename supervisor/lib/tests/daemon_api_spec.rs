use treadmill_supervisor_lib::daemon_api::openapi_spec;

const UPDATE_HINT: &str =
    "UPDATE_SCHEMA=1 cargo test -p treadmill-supervisor-lib --test daemon_api_spec";

#[test]
fn daemon_api_spec_drift() {
    let generated = serde_norway::to_string(&openapi_spec()).expect("serialize openapi spec");
    let dir = std::env::current_dir().unwrap_or_default().join("api-spec");
    let path = dir.join("daemon-api.yaml");

    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(&dir).expect("create api-spec dir");
        std::fs::write(&path, &generated).expect("write openapi snapshot");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "could not read committed spec {}: {err}. Regenerate it with: {UPDATE_HINT}",
            path.display()
        )
    });
    assert_eq!(
        committed,
        generated,
        "daemon API spec drifted from {}. If intentional, regenerate it with: {UPDATE_HINT}",
        path.display()
    );
}
