use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, anyhow, ensure};
use digest_io::IoWrapper;
use oci_spec::image::{
    Descriptor, ImageIndex, ImageIndexBuilder, ImageManifest, MediaType, SCHEMA_VERSION,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use treadmill_rs::image::assemble::{self, Blob, BlobFormat, EMPTY_CONFIG};
use treadmill_rs::image::media_types;
use treadmill_rs::image::parse::TreadmillImage;
use treadmill_rs::image::{Digest, parse};

use crate::layer_arg::{LayerArg, LayerFormatArg};

const OCI_LAYOUT_MARKER: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn create(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let layout = Layout { root: root.into() };
        fs::create_dir_all(layout.blobs_dir())
            .with_context(|| format!("create {}", layout.blobs_dir().display()))?;
        Ok(layout)
    }

    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let layout = Layout { root: root.into() };
        let marker = fs::read_to_string(layout.root.join("oci-layout"))
            .with_context(|| format!("{}: read oci-layout marker", layout.root.display()))?;
        ensure!(
            marker.contains("imageLayoutVersion"),
            "{}: oci-layout marker is malformed: {marker:?}",
            layout.root.display(),
        );
        Ok(layout)
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs").join("sha256")
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blobs_dir().join(digest.hex())
    }

    pub fn manifest(&self) -> anyhow::Result<(Digest, ImageManifest)> {
        let index: ImageIndex = serde_json::from_slice(
            &fs::read(self.root.join("index.json"))
                .with_context(|| format!("{}: read index.json", self.root.display()))?,
        )
        .with_context(|| format!("{}: parse index.json", self.root.display()))?;
        ensure!(
            index.manifests().len() == 1,
            "{}: image layout index should wrap exactly one manifest, found {}",
            self.root.display(),
            index.manifests().len(),
        );

        let digest: Digest = index.manifests()[0]
            .digest()
            .as_ref()
            .parse()
            .with_context(|| format!("{}: manifest descriptor digest", self.root.display()))?;
        let manifest = serde_json::from_slice(
            &fs::read(self.blob_path(&digest))
                .with_context(|| format!("{}: read manifest blob", self.root.display()))?,
        )
        .with_context(|| format!("{}: parse manifest blob", self.root.display()))?;
        Ok((digest, manifest))
    }

    pub fn store_file(&self, path: &Path) -> anyhow::Result<(Digest, u64)> {
        let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut hasher = IoWrapper(Sha256::new());
        let size = std::io::copy(&mut file, &mut hasher).context("hash file")?;
        let digest = finalize(hasher.0);

        let dest = self.blob_path(&digest);
        if !dest.exists() && fs::hard_link(path, &dest).is_err() {
            fs::copy(path, &dest).with_context(|| format!("copy blob to {}", dest.display()))?;
        }
        Ok((digest, size))
    }

    /// Store a layer blob in its given format.
    pub fn store_layer(&self, layer: &LayerArg) -> anyhow::Result<Blob> {
        let path = &layer.path;
        let format = match layer.format {
            LayerFormatArg::Qcow2 => BlobFormat::Qcow2 {
                virtual_size: qcow2_info(path)?.virtual_size,
            },
            LayerFormatArg::Raw => BlobFormat::Raw,
        };
        let (digest, size) = self
            .store_file(path)
            .with_context(|| format!("store layer blob {}", path.display()))?;
        Ok(Blob {
            digest,
            size,
            format,
        })
    }

    pub fn store_blob_from(&self, source: &Layout, digest: &Digest) -> anyhow::Result<()> {
        let dest = self.blob_path(digest);
        if dest.exists() {
            return Ok(());
        }
        let src = source.blob_path(digest);
        if fs::hard_link(&src, &dest).is_err() {
            fs::copy(&src, &dest)
                .with_context(|| format!("copy blob {} to {}", src.display(), dest.display()))?;
        }
        Ok(())
    }

    fn store_bytes(&self, bytes: &[u8]) -> anyhow::Result<(Digest, u64)> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = finalize(hasher);

        let dest = self.blob_path(&digest);
        if !dest.exists() {
            fs::write(&dest, bytes).with_context(|| format!("write blob {}", dest.display()))?;
        }
        Ok((digest, bytes.len() as u64))
    }

    /// Write `image`'s canonical manifest and point the layout's index at it.
    pub fn write_image(&self, image: &TreadmillImage) -> anyhow::Result<()> {
        self.store_bytes(EMPTY_CONFIG)?;

        let json = assemble::manifest_bytes(&image.to_manifest());
        let (digest, size) = self.store_bytes(&json)?;

        let mut descriptor = Descriptor::new(MediaType::ImageManifest, size, oci_digest(&digest)?);
        descriptor.set_artifact_type(Some(MediaType::Other(
            media_types::IMAGE_ARTIFACT_TYPE.to_string(),
        )));
        let index = ImageIndexBuilder::default()
            .schema_version(SCHEMA_VERSION)
            .media_type(MediaType::ImageIndex)
            .manifests(vec![descriptor])
            .build()
            .context("build image index")?;

        fs::write(
            self.root.join("index.json"),
            serde_json_canonicalizer::to_vec(&index).context("serialize index")?,
        )
        .context("write index.json")?;
        fs::write(self.root.join("oci-layout"), OCI_LAYOUT_MARKER).context("write oci-layout")?;

        Ok(())
    }

    /// Drop every blob the manifest does not reference. An append that writes
    /// into its own lower layout leaves the lower's manifest behind.
    pub fn prune(&self) -> anyhow::Result<()> {
        let (manifest_digest, manifest) = self.manifest()?;

        let mut keep: HashSet<String> = HashSet::new();
        keep.insert(manifest_digest.hex());
        keep.insert(strip_algorithm(manifest.config().digest().as_ref()).to_string());
        for layer in manifest.layers() {
            keep.insert(strip_algorithm(layer.digest().as_ref()).to_string());
        }

        for entry in fs::read_dir(self.blobs_dir())
            .with_context(|| format!("read {}", self.blobs_dir().display()))?
        {
            let entry = entry.context("read blob dir entry")?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) {
                fs::remove_file(entry.path())
                    .with_context(|| format!("prune blob {}", entry.path().display()))?;
            }
        }
        Ok(())
    }
}

fn strip_algorithm(digest: &str) -> &str {
    digest.split_once(':').map_or(digest, |(_, hex)| hex)
}

fn finalize(hasher: Sha256) -> Digest {
    let out = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&out);
    Digest::from_sha256(bytes)
}

fn oci_digest(d: &Digest) -> anyhow::Result<oci_spec::image::Digest> {
    oci_spec::image::Digest::from_str(&d.encoded())
        .map_err(|e| anyhow!("invalid OCI digest {}: {e}", d.encoded()))
}

pub fn read_image(layout: &Layout) -> anyhow::Result<parse::TreadmillImage> {
    let (_, manifest) = layout.manifest()?;
    parse::parse_image(&manifest)
        .map_err(|e| anyhow!("manifest does not reparse as a Treadmill image: {e}"))
}

/// What `qemu-img info` reports about a qcow2 image.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Qcow2Info {
    pub virtual_size: u64,
    pub backing_filename: Option<String>,
}

/// Read a qcow2 image's metadata with `qemu-img info`, which fails if the file
/// is not a qcow2 image.
pub fn qcow2_info(path: &Path) -> anyhow::Result<Qcow2Info> {
    // `--image-opts` so that no part of the path is read as a protocol prefix
    // or an option; a literal comma is escaped by doubling it.
    let image_opts = format!(
        "driver=qcow2,file.driver=file,file.filename={}",
        path.to_str()
            .with_context(|| format!("{}: path is not valid UTF-8", path.display()))?
            .replace(',', ",,"),
    );
    let output = Command::new("qemu-img")
        .args(["info", "--output=json", "--image-opts", &image_opts])
        .output()
        .context("run qemu-img info")?;
    ensure!(
        output.status.success(),
        "{}: qemu-img info failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim(),
    );
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{}: parse qemu-img info output", path.display()))
}

pub fn verify_blob_digest(path: &Path, digest: &Digest) -> anyhow::Result<()> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = IoWrapper(Sha256::new());
    std::io::copy(&mut file, &mut hasher).context("hash blob")?;
    let actual = finalize(hasher.0);
    ensure!(
        actual == *digest,
        "blob {} hashes to {} but is referenced as {}",
        path.display(),
        actual.encoded(),
        digest.encoded(),
    );
    Ok(())
}
