//! Drift-guard for the producer-side image/group layouts (the `images/` tree).
//!
//! `images/lib/{media-types,mk-treadmill-image,mk-group-index}.nix` hand-write
//! the Treadmill media types and `ci.treadmill.*` annotations. This test is the
//! backstop against those Nix string literals drifting from the Rust constants:
//! it reparses every built layout through the REAL
//! [`treadmill_rs::image::parse`] view and asserts the structural invariants the
//! rest of the system relies on (see `doc/images-oci-migration-plan.md` §6.1).
//!
//! The set of layouts to check — and each one's expected shape — is supplied
//! out-of-band via a JSON spec whose path is in `TML_IMAGES_PARSE_SPEC` (the
//! Nix `images-parse` derivation writes it, pointing at the built layouts).
//! With the variable unset — a plain `cargo test` — the test skips, so it costs
//! nothing outside the check that can actually provide the artifacts (same
//! posture as the `tiny_efi` fixture test).

use std::path::{Path, PathBuf};

use oci_spec::image::{ImageIndex, ImageManifest};
use serde::Deserialize;

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::{Digest, parse};

/// The JSON spec handed to us in `TML_IMAGES_PARSE_SPEC`.
#[derive(Debug, Deserialize)]
struct Spec {
    #[serde(default)]
    images: Vec<ImageSpec>,
    #[serde(default)]
    groups: Vec<GroupSpec>,
}

#[derive(Debug, Deserialize)]
struct ImageSpec {
    name: String,
    path: PathBuf,
    /// Number of `role=root` layers expected in the backing chain.
    root_layers: usize,
    /// Number of `role=boot` layers expected (0 for qemu images, 1 for netboot).
    #[serde(default)]
    boot_layers: usize,
    /// Expected `org.opencontainers.image.title`, if asserted.
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GroupSpec {
    name: String,
    path: PathBuf,
    members: Vec<MemberSpec>,
}

#[derive(Debug, Deserialize)]
struct MemberSpec {
    #[serde(default)]
    required_host_tags: Vec<String>,
}

fn blob_path(layout: &Path, digest: &Digest) -> PathBuf {
    layout.join("blobs").join("sha256").join(digest.hex())
}

/// Read `<layout>/index.json` and return the single image manifest it wraps.
fn read_single_manifest(layout: &Path) -> ImageManifest {
    let oci_layout = std::fs::read_to_string(layout.join("oci-layout"))
        .unwrap_or_else(|e| panic!("{}: read oci-layout marker: {e}", layout.display()));
    assert!(
        oci_layout.contains("imageLayoutVersion"),
        "{}: oci-layout marker is malformed: {oci_layout:?}",
        layout.display(),
    );

    let index: ImageIndex = serde_json::from_slice(
        &std::fs::read(layout.join("index.json"))
            .unwrap_or_else(|e| panic!("{}: read index.json: {e}", layout.display())),
    )
    .unwrap_or_else(|e| panic!("{}: parse index.json: {e}", layout.display()));
    assert_eq!(
        index.manifests().len(),
        1,
        "{}: image layout index should wrap exactly one manifest",
        layout.display(),
    );

    let digest: Digest = index.manifests()[0]
        .digest()
        .as_ref()
        .parse()
        .unwrap_or_else(|e| panic!("{}: manifest descriptor digest: {e}", layout.display()));
    serde_json::from_slice(
        &std::fs::read(blob_path(layout, &digest))
            .unwrap_or_else(|e| panic!("{}: read manifest blob: {e}", layout.display())),
    )
    .unwrap_or_else(|e| panic!("{}: parse manifest blob: {e}", layout.display()))
}

fn check_image(spec: &ImageSpec) {
    let what = &spec.name;
    let manifest = read_single_manifest(&spec.path);
    let image = parse::parse_image(&manifest)
        .unwrap_or_else(|e| panic!("{what}: manifest does not reparse as a Treadmill image: {e}"));

    if let Some(title) = &spec.title {
        assert_eq!(
            image.title.as_deref(),
            Some(title.as_str()),
            "{what}: unexpected image title",
        );
    }

    let roots: Vec<_> = image
        .layers
        .iter()
        .filter(|l| l.role == Some(Role::Root))
        .collect();
    let boots: Vec<_> = image
        .layers
        .iter()
        .filter(|l| l.role == Some(Role::Boot))
        .collect();
    assert_eq!(
        roots.len(),
        spec.root_layers,
        "{what}: wrong root-layer count"
    );
    assert_eq!(
        boots.len(),
        spec.boot_layers,
        "{what}: wrong boot-layer count"
    );

    // Chain wiring: first root has no lower; each subsequent root backs onto its
    // predecessor; head names the last root layer (no baked backing path is even
    // representable here — the chain is digest references only).
    for (i, layer) in roots.iter().enumerate() {
        if i == 0 {
            assert_eq!(
                layer.lower, None,
                "{what}: first root layer must have no lower"
            );
        } else {
            assert_eq!(
                layer.lower.as_ref(),
                Some(&roots[i - 1].digest),
                "{what}: root layer {i} must back onto its predecessor",
            );
        }
    }
    assert_eq!(
        image.head,
        roots.last().expect("at least one root layer").digest,
        "{what}: head must name the last root layer",
    );

    // Boot layers are standalone: a role but no chain links.
    for layer in &boots {
        assert_eq!(layer.lower, None, "{what}: boot layer must have no lower");
        assert_ne!(
            image.head, layer.digest,
            "{what}: boot layer must not be head"
        );
    }

    // Every referenced blob is present at the size its descriptor claims.
    for layer in &image.layers {
        let path = blob_path(&spec.path, &layer.digest);
        let meta = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{what}: layer blob {path:?} missing: {e}"));
        assert_eq!(
            meta.len(),
            layer.size,
            "{what}: layer blob {path:?} size disagrees with its descriptor",
        );
    }
}

fn check_group(spec: &GroupSpec) {
    let what = &spec.name;
    let index: ImageIndex = serde_json::from_slice(
        &std::fs::read(spec.path.join("index.json"))
            .unwrap_or_else(|e| panic!("{what}: read group index.json: {e}")),
    )
    .unwrap_or_else(|e| panic!("{what}: parse group index.json: {e}"));

    let members = parse::parse_group(&index)
        .unwrap_or_else(|e| panic!("{what}: index does not reparse as a Treadmill group: {e}"));
    assert_eq!(
        members.len(),
        spec.members.len(),
        "{what}: wrong group member count",
    );

    for (member, expected) in members.iter().zip(&spec.members) {
        assert_eq!(
            member.required_host_tags, expected.required_host_tags,
            "{what}: member required-host-tags disagree with the groups.nix table",
        );
        // The member descriptor's manifest must be present in the group's CAS
        // (self-contained layout — a single skopeo copy pushes everything).
        let path = blob_path(&spec.path, &member.digest);
        std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{what}: member manifest blob {path:?} missing: {e}"));
    }
}

#[test]
fn built_layouts_reparse_as_treadmill_images_and_groups() {
    let Some(spec_path) = std::env::var_os("TML_IMAGES_PARSE_SPEC") else {
        eprintln!("TML_IMAGES_PARSE_SPEC unset; skipping images-parse drift guard");
        return;
    };
    let spec: Spec = serde_json::from_slice(
        &std::fs::read(&spec_path).expect("read TML_IMAGES_PARSE_SPEC file"),
    )
    .expect("parse TML_IMAGES_PARSE_SPEC as JSON");

    for image in &spec.images {
        check_image(image);
    }
    for group in &spec.groups {
        check_group(group);
    }
}
