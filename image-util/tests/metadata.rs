use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

fn run(root: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_image-util"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn manifest(layout: &Path) -> Value {
    let index: Value =
        serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
    let descriptor = &index["manifests"][0];
    let digest = descriptor["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let bytes = fs::read(layout.join("blobs/sha256").join(digest)).unwrap();
    assert_eq!(
        Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        digest
    );
    assert_eq!(bytes.len() as u64, descriptor["size"].as_u64().unwrap());
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn build_metadata_survives_assembly_and_append() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("disk.raw"), b"a raw disk layer").unwrap();
    run(
        root,
        &[
            "assemble",
            "-o",
            "base",
            "--title",
            "Base",
            "--layer",
            "disk=raw:disk.raw",
            "--version",
            "20260925T143012Z-0123456",
            "--description",
            "A plain-text summary.",
            "--created",
            "2026-09-25T14:30:12Z",
            "--revision",
            "0123456789abcdef",
            "--documentation",
            "https://github.com/example/images/blob/0123456789abcdef/images/base/README.md",
        ],
    );
    run(root, &["verify", "base"]);
    let base = manifest(&root.join("base"));
    let annotations = &base["annotations"];
    assert_eq!(
        annotations["org.opencontainers.image.created"],
        "2026-09-25T14:30:12Z"
    );
    assert_eq!(
        annotations["org.opencontainers.image.revision"],
        "0123456789abcdef"
    );
    assert_eq!(
        annotations["org.opencontainers.image.documentation"],
        "https://github.com/example/images/blob/0123456789abcdef/images/base/README.md"
    );
    assert_eq!(
        annotations["org.opencontainers.image.description"],
        "A plain-text summary."
    );
    assert_eq!(
        annotations["org.opencontainers.image.version"],
        "20260925T143012Z-0123456"
    );

    run(root, &["append", "--lower", "base", "-o", "inherited"]);
    let inherited = manifest(&root.join("inherited"));
    for key in [
        "created",
        "revision",
        "documentation",
        "description",
        "version",
    ] {
        let key = format!("org.opencontainers.image.{key}");
        assert_eq!(inherited["annotations"][&key], annotations[&key]);
    }

    run(
        root,
        &[
            "append",
            "--lower",
            "base",
            "-o",
            "derived",
            "--created",
            "2026-09-25T15:00:00Z",
            "--revision",
            "fedcba9876543210",
            "--documentation",
            "https://github.com/example/images/blob/fedcba9876543210/images/derived/README.md",
        ],
    );
    run(root, &["verify", "derived"]);
    let derived = manifest(&root.join("derived"));
    assert_eq!(
        derived["annotations"]["org.opencontainers.image.created"],
        "2026-09-25T15:00:00Z"
    );
    assert_eq!(
        derived["annotations"]["org.opencontainers.image.revision"],
        "fedcba9876543210"
    );
    assert_eq!(
        derived["annotations"]["org.opencontainers.image.documentation"],
        "https://github.com/example/images/blob/fedcba9876543210/images/derived/README.md"
    );
    assert_eq!(derived["layers"], base["layers"]);
    assert_eq!(manifest(&root.join("base")), base);
}
