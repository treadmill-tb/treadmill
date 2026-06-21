//! `image-util`: assemble and validate Treadmill OCI image layouts.
//!
//! Two subcommands, sharing the `treadmill_rs::image` model so the producer and
//! the consumer cannot drift:
//!
//! * `assemble` — store blob files into a content-addressed OCI layout and
//!   write the manifest/index, deriving the backing-chain wiring from layer
//!   order + role via [`treadmill_rs::image::assemble`].
//! * `parse` — reparse ONE built layout through the real
//!   [`treadmill_rs::image::parse`] view and assert the structural invariants
//!   the rest of the system relies on.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Context, anyhow, ensure};
use clap::{Parser, Subcommand};
use oci_spec::image::{
    Descriptor, ImageIndex, ImageIndexBuilder, ImageManifest, MediaType, SCHEMA_VERSION,
};
use sha2::{Digest as _, Sha256};

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::assemble::{self, ImageMeta, LayerSpec};
use treadmill_rs::image::media_types;
use treadmill_rs::image::{Digest, parse};

#[derive(Debug, Parser)]
#[command(name = "image-util")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Assemble blob files into a validated Treadmill OCI image layout.
    Assemble(AssembleArgs),
    /// Validate a built layout against its expected shape.
    Parse(ParseArgs),
}

/// Assemble an ordered list of layer blobs into an OCI layout directory.
#[derive(Debug, Parser)]
struct AssembleArgs {
    /// Output OCI layout directory (holds `oci-layout`, `index.json`, `blobs/`;
    /// created if absent).
    #[arg(short = 'o', long)]
    out: PathBuf,

    /// `org.opencontainers.image.title`.
    #[arg(long)]
    title: String,

    /// `org.opencontainers.image.version` (optional).
    #[arg(long)]
    version: Option<String>,

    /// `org.opencontainers.image.description` (optional).
    #[arg(long)]
    description: Option<String>,

    /// A layer blob as `ROLE=PATH` (ROLE is `root` or `boot`). Repeatable, and
    /// ORDER IS SIGNIFICANT — it defines the backing chain (see
    /// `treadmill_rs::image::assemble`).
    #[arg(long = "layer", value_name = "ROLE=PATH", required = true)]
    layers: Vec<String>,

    /// Human-readable label used in messages (defaults to the title).
    #[arg(long)]
    name: Option<String>,
}

/// Validate a built Treadmill OCI image layout against its expected shape.
#[derive(Debug, Parser)]
struct ParseArgs {
    /// Path to the OCI image layout directory.
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
    match Cli::parse().command {
        Command::Assemble(args) => {
            let what = args.name.clone().unwrap_or_else(|| args.title.clone());
            finish(&what, assemble_layout(&args))
        }
        Command::Parse(args) => {
            let what = args
                .name
                .clone()
                .unwrap_or_else(|| args.layout.display().to_string());
            finish(&what, check_image(&args))
        }
    }
}

fn finish(what: &str, result: anyhow::Result<()>) -> ExitCode {
    match result {
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

// ---------------------------------------------------------------------------
// assemble
// ---------------------------------------------------------------------------

fn assemble_layout(args: &AssembleArgs) -> anyhow::Result<()> {
    let blobs = args.out.join("blobs").join("sha256");
    fs::create_dir_all(&blobs).with_context(|| format!("create {}", blobs.display()))?;

    let mut specs = Vec::with_capacity(args.layers.len());
    for raw in &args.layers {
        let (role_str, path_str) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("--layer must be ROLE=PATH, got {raw:?}"))?;
        let role = Role::from_str(role_str).map_err(|e| anyhow!("invalid layer role {:?}", e.0))?;
        let path = Path::new(path_str);

        let (digest, size) =
            store_file(path, &blobs).with_context(|| format!("store layer blob {path:?}"))?;
        let virtual_size = match role {
            Role::Root => Some(
                qcow2_virtual_size(path)
                    .with_context(|| format!("read qcow2 virtual size of {path:?}"))?,
            ),
            Role::Boot => None,
        };
        specs.push(LayerSpec {
            digest,
            size,
            role,
            virtual_size,
        });
    }

    let meta = ImageMeta {
        title: Some(args.title.clone()),
        version: args.version.clone(),
        description: args.description.clone(),
    };
    let manifest = assemble::build_manifest(&specs, &meta).context("assemble manifest")?;

    // Empty config blob `{}` (the manifest's config descriptor references it).
    store_bytes(b"{}", &blobs)?;

    // Manifest blob, then the index that wraps it.
    let manifest_json = serde_json::to_vec(&manifest).context("serialize manifest")?;
    let (manifest_digest, manifest_size) = store_bytes(&manifest_json, &blobs)?;

    let index = build_index(&manifest_digest, manifest_size)?;
    fs::write(
        args.out.join("index.json"),
        serde_json::to_vec(&index).context("serialize index")?,
    )
    .context("write index.json")?;
    fs::write(
        args.out.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .context("write oci-layout")?;

    Ok(())
}

fn build_index(manifest_digest: &Digest, manifest_size: u64) -> anyhow::Result<ImageIndex> {
    let mut descriptor = Descriptor::new(
        MediaType::ImageManifest,
        manifest_size,
        oci_digest(manifest_digest)?,
    );
    descriptor.set_artifact_type(Some(MediaType::Other(
        media_types::IMAGE_ARTIFACT_TYPE.to_string(),
    )));

    ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(vec![descriptor])
        .build()
        .context("build image index")
}

/// Hash a file (streaming), copy it into the content-addressed blob store, and
/// return its digest + size. A blob already present (same digest) is not
/// recopied.
fn store_file(path: &Path, blobs: &Path) -> anyhow::Result<(Digest, u64)> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher).context("hash file")?;
    let digest = finalize(hasher);

    let dest = blobs.join(digest.hex());
    if !dest.exists() {
        fs::copy(path, &dest).with_context(|| format!("copy blob to {}", dest.display()))?;
    }
    Ok((digest, size))
}

/// As [`store_file`], for an in-memory blob (config, manifest).
fn store_bytes(bytes: &[u8], blobs: &Path) -> anyhow::Result<(Digest, u64)> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = finalize(hasher);

    let dest = blobs.join(digest.hex());
    if !dest.exists() {
        fs::write(&dest, bytes).with_context(|| format!("write blob {}", dest.display()))?;
    }
    Ok((digest, bytes.len() as u64))
}

fn finalize(hasher: Sha256) -> Digest {
    let out = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&out);
    Digest::from_sha256(bytes)
}

/// Read a qcow2's virtual disk size from its header: an 8-byte big-endian field
/// at offset 24 (after the `QFI\xfb` magic). Avoids shelling out to `qemu-img`.
fn qcow2_virtual_size(path: &Path) -> anyhow::Result<u64> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).context("read qcow2 magic")?;
    ensure!(
        &magic == b"QFI\xfb",
        "{path:?} is not a qcow2 file (bad magic)"
    );
    file.seek(SeekFrom::Start(24))
        .context("seek to size field")?;
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf).context("read size field")?;
    Ok(u64::from_be_bytes(buf))
}

fn oci_digest(d: &Digest) -> anyhow::Result<oci_spec::image::Digest> {
    oci_spec::image::Digest::from_str(&d.encoded())
        .map_err(|e| anyhow!("invalid OCI digest {}: {e}", d.encoded()))
}

// ---------------------------------------------------------------------------
// parse (validate)
// ---------------------------------------------------------------------------

fn blob_path(layout: &Path, digest: &Digest) -> PathBuf {
    layout.join("blobs").join("sha256").join(digest.hex())
}

/// Read `<layout>/index.json` and return the single image manifest it wraps.
fn read_single_manifest(layout: &Path) -> anyhow::Result<ImageManifest> {
    let oci_layout = fs::read_to_string(layout.join("oci-layout"))
        .with_context(|| format!("{}: read oci-layout marker", layout.display()))?;
    ensure!(
        oci_layout.contains("imageLayoutVersion"),
        "{}: oci-layout marker is malformed: {oci_layout:?}",
        layout.display(),
    );

    let index: ImageIndex = serde_json::from_slice(
        &fs::read(layout.join("index.json"))
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
        &fs::read(blob_path(layout, &digest))
            .with_context(|| format!("{}: read manifest blob", layout.display()))?,
    )
    .with_context(|| format!("{}: parse manifest blob", layout.display()))
}

fn check_image(args: &ParseArgs) -> anyhow::Result<()> {
    let manifest = read_single_manifest(&args.layout)?;
    let image = parse::parse_image(&manifest)
        .map_err(|e| anyhow!("manifest does not reparse as a Treadmill image: {e}"))?;

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
    // predecessor; head names the last root layer.
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
        let meta = fs::metadata(&path).with_context(|| format!("layer blob {path:?} missing"))?;
        ensure!(
            meta.len() == layer.size,
            "layer blob {path:?} size {} disagrees with its descriptor ({})",
            meta.len(),
            layer.size,
        );
    }

    Ok(())
}
