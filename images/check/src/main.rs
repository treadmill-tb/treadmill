//! `image-check`: validate ONE built Treadmill OCI image layout against the
//! shape its producer recipe declares (the `imageDefs` table in
//! `nix/images.nix`).
//!
//! This is the per-image drift guard for the producer-side image builds. It
//! reparses a single built layout through the real [`treadmill_rs::image::parse`]
//! view and asserts the structural invariants the rest of the system relies on
//! (see `doc/images-oci-migration-plan.md` §6.1). It is the backstop against the
//! Nix string literals in `images/lib/{media-types,mk-treadmill-image}.nix`
//! drifting from the Rust constants.
//!
//! Taking exactly one layout as an argument — instead of a JSON spec listing
//! every built image — lets each image be validated in isolation, so nothing has
//! to build the whole image set at once.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, ensure};
use clap::Parser;
use oci_spec::image::{ImageIndex, ImageManifest};

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::{Digest, parse};

/// Validate a built Treadmill OCI image layout against its expected shape.
#[derive(Debug, Parser)]
#[command(name = "image-check")]
struct Args {
    /// Path to the OCI image layout directory (holds `oci-layout`,
    /// `index.json`, and `blobs/`).
    layout: PathBuf,

    /// Expected number of `role=root` layers in the backing chain.
    #[arg(long)]
    root_layers: usize,

    /// Expected number of `role=boot` layers (0 for qemu images, 1 for netboot).
    #[arg(long, default_value_t = 0)]
    boot_layers: usize,

    /// Expected `org.opencontainers.image.title`; asserted only when given.
    #[arg(long)]
    title: Option<String>,

    /// Human-readable label used in messages (defaults to the layout path).
    #[arg(long)]
    name: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let what = args
        .name
        .clone()
        .unwrap_or_else(|| args.layout.display().to_string());

    match check_image(&args) {
        Ok(()) => {
            println!("{what}: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{what}: FAILED: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn blob_path(layout: &Path, digest: &Digest) -> PathBuf {
    layout.join("blobs").join("sha256").join(digest.hex())
}

/// Read `<layout>/index.json` and return the single image manifest it wraps.
fn read_single_manifest(layout: &Path) -> anyhow::Result<ImageManifest> {
    let oci_layout = std::fs::read_to_string(layout.join("oci-layout"))
        .with_context(|| format!("{}: read oci-layout marker", layout.display()))?;
    ensure!(
        oci_layout.contains("imageLayoutVersion"),
        "{}: oci-layout marker is malformed: {oci_layout:?}",
        layout.display(),
    );

    let index: ImageIndex = serde_json::from_slice(
        &std::fs::read(layout.join("index.json"))
            .with_context(|| format!("{}: read index.json", layout.display()))?,
    )
    .with_context(|| format!("{}: parse index.json", layout.display()))?;
    ensure!(
        index.manifests().len() == 1,
        "{}: image layout index should wrap exactly one manifest",
        layout.display(),
    );

    let digest: Digest = index.manifests()[0]
        .digest()
        .as_ref()
        .parse()
        .with_context(|| format!("{}: manifest descriptor digest", layout.display()))?;
    serde_json::from_slice(
        &std::fs::read(blob_path(layout, &digest))
            .with_context(|| format!("{}: read manifest blob", layout.display()))?,
    )
    .with_context(|| format!("{}: parse manifest blob", layout.display()))
}

fn check_image(args: &Args) -> anyhow::Result<()> {
    let manifest = read_single_manifest(&args.layout)?;
    let image = parse::parse_image(&manifest)
        .map_err(|e| anyhow::anyhow!("manifest does not reparse as a Treadmill image: {e}"))?;

    if let Some(title) = &args.title {
        ensure!(
            image.title.as_deref() == Some(title.as_str()),
            "unexpected image title: got {:?}, expected {title:?}",
            image.title,
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
    ensure!(
        roots.len() == args.root_layers,
        "wrong root-layer count: got {}, expected {}",
        roots.len(),
        args.root_layers,
    );
    ensure!(
        boots.len() == args.boot_layers,
        "wrong boot-layer count: got {}, expected {}",
        boots.len(),
        args.boot_layers,
    );

    // Chain wiring: first root has no lower; each subsequent root backs onto its
    // predecessor; head names the last root layer (no baked backing path is even
    // representable here — the chain is digest references only).
    for (i, layer) in roots.iter().enumerate() {
        if i == 0 {
            ensure!(layer.lower.is_none(), "first root layer must have no lower");
        } else {
            ensure!(
                layer.lower.as_ref() == Some(&roots[i - 1].digest),
                "root layer {i} must back onto its predecessor",
            );
        }
    }
    let head_root = roots.last().context("image has no root layer")?;
    ensure!(
        image.head == head_root.digest,
        "head must name the last root layer",
    );

    // Boot layers are standalone: a role but no chain links.
    for layer in &boots {
        ensure!(layer.lower.is_none(), "boot layer must have no lower");
        ensure!(image.head != layer.digest, "boot layer must not be head");
    }

    // Every referenced blob is present at the size its descriptor claims.
    for layer in &image.layers {
        let path = blob_path(&args.layout, &layer.digest);
        let meta =
            std::fs::metadata(&path).with_context(|| format!("layer blob {path:?} missing"))?;
        ensure!(
            meta.len() == layer.size,
            "layer blob {path:?} size {} disagrees with its descriptor ({})",
            meta.len(),
            layer.size,
        );
    }

    Ok(())
}
