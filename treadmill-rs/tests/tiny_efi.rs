//! Round-trips the committed `tiny-efi` OCI fixture through our `oci-spec` view.
//!
//! The fixture is built by `nix/tiny-efi.nix` (see
//! `doc/oci-image-migration-plan.md` §12.2): a two-layer qcow2 backing chain
//! packaged as a standard OCI image layout. This test reparses that real
//! wire-format manifest with [`treadmill_rs::image::parse`] and asserts the
//! Treadmill view it projects — proving the layout the fixture emits and the
//! types the rest of the system consumes agree.
//!
//! The fixture path is supplied out-of-band in `TINY_EFI_IMAGE` (the Nix check
//! `tiny-efi-image` sets it to the built layout). With the variable unset — a
//! plain `cargo test` — the test skips, so it costs nothing outside the check
//! that can actually provide the artifact.

use std::path::{Path, PathBuf};

use oci_spec::image::{ImageIndex, ImageManifest};

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::{Digest, media_types, parse};

/// 16 MiB — the ESP size every layer in the chain shares (see `nix/tiny-efi.nix`).
const EXPECTED_VIRTUAL_SIZE: u64 = 16 * 1024 * 1024;

fn fixture_dir() -> Option<PathBuf> {
    std::env::var_os("TINY_EFI_IMAGE").map(PathBuf::from)
}

/// Resolve a `sha256:<hex>` digest to its `blobs/sha256/<hex>` path in `layout`.
fn blob_path(layout: &Path, digest: &Digest) -> PathBuf {
    layout.join("blobs").join("sha256").join(digest.hex())
}

#[test]
fn tiny_efi_fixture_reparses_as_treadmill_image() {
    let Some(layout) = fixture_dir() else {
        eprintln!("TINY_EFI_IMAGE unset; skipping tiny-efi fixture reparse");
        return;
    };

    // The layout must announce itself as an OCI image layout.
    let oci_layout =
        std::fs::read_to_string(layout.join("oci-layout")).expect("read oci-layout marker");
    assert!(
        oci_layout.contains("imageLayoutVersion"),
        "oci-layout marker is malformed: {oci_layout:?}",
    );

    // index.json -> the single image manifest descriptor.
    let index: ImageIndex =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).expect("read index.json"))
            .expect("parse index.json as an OCI image index");
    assert_eq!(
        index.manifests().len(),
        1,
        "fixture index should reference exactly one manifest",
    );
    let manifest_desc = &index.manifests()[0];
    let manifest_digest: Digest = manifest_desc
        .digest()
        .as_ref()
        .parse()
        .expect("manifest descriptor digest parses");

    // Read + parse the manifest blob, then project it onto the Treadmill view.
    let manifest: ImageManifest = serde_json::from_slice(
        &std::fs::read(blob_path(&layout, &manifest_digest)).expect("read manifest blob"),
    )
    .expect("parse manifest blob as an OCI image manifest");
    let image = parse::parse_image(&manifest).expect("manifest reparses as a Treadmill image");

    // Two-layer chain: base (no lower) then overlay (lower = base), head = overlay.
    assert_eq!(image.title.as_deref(), Some("tiny-efi"));
    assert_eq!(image.layers.len(), 2, "expected a base + overlay chain");

    let base = &image.layers[0];
    let overlay = &image.layers[1];

    assert_eq!(
        image.head, overlay.digest,
        "head must name the overlay layer"
    );
    assert_eq!(base.lower, None, "base layer has no lower");
    assert_eq!(
        overlay.lower,
        Some(base.digest),
        "overlay must back onto the base layer",
    );

    for layer in &image.layers {
        assert_eq!(layer.role, Some(Role::Root));
        assert_eq!(layer.media_type, media_types::DISK_QCOW2);
        assert_eq!(layer.virtual_size, Some(EXPECTED_VIRTUAL_SIZE));

        // Every referenced blob is actually present in the layout, at the size
        // the descriptor claims (a cheap end-to-end check on the CAS).
        let path = blob_path(&layout, &layer.digest);
        let meta = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("layer blob {path:?} is missing: {e}"));
        assert_eq!(
            meta.len(),
            layer.size,
            "layer blob {path:?} size disagrees with its descriptor",
        );
    }
}
