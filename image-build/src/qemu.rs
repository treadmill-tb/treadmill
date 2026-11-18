//! The qcow2 work a build is made of.
//!
//! Every layer of a Treadmill image is a qcow2 delta against the layer below
//! it, and all of that is `qemu-img`. None of it needs root: these are
//! ordinary files the build owns.
//!
//! One convention runs through the whole module. A shipped blob's backing
//! file is the **empty string**, never a path: images are assembled at launch
//! from wherever the supervisor put the blobs, and a baked-in path would be
//! wrong everywhere but the machine that built it. So blobs come out of here
//! unbaked ([`finalize_delta`]), and have to be relinked before anything can
//! be stacked on them ([`link_chain`]).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;

use crate::exec;
use crate::layout::qcow2_info;

/// How every shipped blob is compressed.
///
/// zstd rather than qcow2's default zlib, and 128K clusters rather than 64K:
/// these blobs are written once and read many times over a network, so a
/// slower, denser compression is the right trade.
const COMPRESS: &[&str] = &["-c", "-o", "compression_type=zstd,cluster_size=128k"];

/// Create an empty overlay backed by `lower`.
pub fn create_overlay(out: &Path, lower: &Path) -> Result<()> {
    exec::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2", "-b"])
            .arg(lower)
            .args(["-F", "qcow2"])
            .arg(out),
    )
    .with_context(|| format!("create overlay {}", out.display()))
}

/// Grow `overlay` by `extra` bytes.
pub fn grow_overlay(overlay: &Path, extra: u64) -> Result<()> {
    exec::run(
        Command::new("qemu-img")
            .args(["resize", "-q", "-f", "qcow2"])
            .arg(overlay)
            .arg(format!("+{extra}")),
    )
    .with_context(|| format!("grow {}", overlay.display()))
}

/// Convert a qcow2 image into a compressed, unbaked blob.
pub fn convert_whole(source: &Path, out: &Path) -> Result<()> {
    exec::run(
        Command::new("qemu-img")
            .arg("convert")
            .args(COMPRESS)
            .args(["-f", "qcow2", "-O", "qcow2"])
            .arg(source)
            .arg(out),
    )
    .with_context(|| format!("convert {} to {}", source.display(), out.display()))?;
    unbake(out)
}

/// Write the clusters of `overlay` that differ from `lower` into `out`, as a
/// compressed, unbaked delta.
pub fn finalize_delta(overlay: &Path, lower: &Path, out: &Path) -> Result<()> {
    exec::run(
        Command::new("qemu-img")
            .arg("convert")
            .args(COMPRESS)
            .args(["-f", "qcow2", "-O", "qcow2", "-B"])
            .arg(lower)
            .args(["-F", "qcow2"])
            .arg(overlay)
            .arg(out),
    )
    .with_context(|| format!("finalize delta {}", out.display()))?;
    unbake(out)
}

/// Strip a blob's backing filename, restoring the unbaked-chain invariant.
///
/// `-u` is "unsafe" only in that it changes the metadata without rewriting
/// data, which is exactly what is wanted: the clusters are already correct
/// relative to the lower, and the supplied chain at launch is the same one.
pub fn unbake(blob: &Path) -> Result<()> {
    exec::run(
        Command::new("qemu-img")
            .args(["rebase", "-u", "-b", "", "-f", "qcow2"])
            .arg(blob),
    )
    .with_context(|| format!("unbake {}", blob.display()))
}

/// Whether `overlay` holds nothing that `lower` does not.
///
/// An unchanged volume must get no layer at all. Its delta would be empty,
/// and therefore byte-identical to every other empty delta — the same digest
/// turning up in unrelated images, which is a confusing thing to have to
/// debug later.
pub fn volume_unchanged(overlay: &Path, lower: &Path) -> Result<bool> {
    let overlay_size = qcow2_info(overlay)?.virtual_size;
    let lower_size = qcow2_info(lower)?.virtual_size;
    if overlay_size != lower_size {
        return Ok(false);
    }

    let code = exec::status_code(
        Command::new("qemu-img")
            .args(["compare", "-q", "-f", "qcow2", "-F", "qcow2"])
            .arg(overlay)
            .arg(lower),
    )?;
    match code {
        0 => Ok(true),
        1 => Ok(false),
        other => bail!(
            "comparing {} against {} failed with status {other}",
            overlay.display(),
            lower.display(),
        ),
    }
}

/// Relink a chain of unbaked blobs so that something can be stacked on it.
///
/// The blobs ship with no backing filename, so on their own they are a pile
/// of unrelated images. Each one above the base is copied into `work` and
/// rebased onto the one before it; the base is used where it lies, since
/// nothing has to be written to it. Returns the path of the head.
///
/// The copies are what make this safe to do to blobs in a shared cache: the
/// originals keep their empty backing file.
pub fn link_chain(work: &Path, tag: &str, blobs: &[PathBuf]) -> Result<PathBuf> {
    let (base, rest) = blobs
        .split_first()
        .context("a chain needs at least one blob")?;
    let mut previous = base.clone();
    for (index, blob) in rest.iter().enumerate() {
        let copy = work.join(format!("chain-{tag}-{}.qcow2", index + 1));
        // fs::copy uses copy_file_range, so this is a reflink on a file
        // system that can do one, and these blobs are large.
        fs::copy(blob, &copy)
            .with_context(|| format!("copy {} to {}", blob.display(), copy.display()))?;
        let mut permissions = fs::metadata(&copy)
            .with_context(|| format!("stat {}", copy.display()))?
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&copy, permissions)
            .with_context(|| format!("make {} writable", copy.display()))?;

        exec::run(
            Command::new("qemu-img")
                .args(["rebase", "-u", "-b"])
                .arg(&previous)
                .args(["-F", "qcow2", "-f", "qcow2"])
                .arg(&copy),
        )
        .with_context(|| format!("relink {}", copy.display()))?;
        previous = copy;
    }
    Ok(previous)
}

/// One partition of a raw disk image, in bytes.
#[derive(Debug, Clone, Copy)]
pub struct Partition {
    pub offset: u64,
    pub size: u64,
}

#[derive(Deserialize)]
struct SfdiskOutput {
    partitiontable: PartitionTable,
}

#[derive(Deserialize)]
struct PartitionTable {
    #[serde(default = "default_sector_size")]
    sectorsize: u64,
    #[serde(default)]
    partitions: Vec<SfdiskPartition>,
}

fn default_sector_size() -> u64 {
    512
}

#[derive(Deserialize)]
struct SfdiskPartition {
    start: u64,
    size: u64,
}

/// Where partition `number` (1-based) of a raw image lies.
pub fn partition_of(image: &Path, number: usize) -> Result<Partition> {
    let json = exec::capture(Command::new("sfdisk").arg("-J").arg(image))
        .with_context(|| format!("read the partition table of {}", image.display()))?;
    let output: SfdiskOutput = serde_json::from_str(&json)
        .with_context(|| format!("parse sfdisk output for {}", image.display()))?;
    let table = output.partitiontable;
    let partition = table
        .partitions
        .get(number - 1)
        .with_context(|| format!("{} has no partition {number}", image.display()))?;
    Ok(Partition {
        offset: partition.start * table.sectorsize,
        size: partition.size * table.sectorsize,
    })
}

/// Convert one partition of a raw image into a compressed, unbaked qcow2.
///
/// Reads the partition as a slice of the file rather than going through a loop
/// device, which keeps this unprivileged.
pub fn convert_partition(image: &Path, number: usize, out: &Path) -> Result<()> {
    let partition = partition_of(image, number)?;
    let filename = image
        .to_str()
        .with_context(|| format!("{}: path is not valid UTF-8", image.display()))?
        // qemu's keyval parser reads a comma as a separator; a literal one is
        // escaped by doubling it.
        .replace(',', ",,");
    let image_opts = format!(
        "driver=raw,offset={},size={},file.driver=file,file.filename={filename}",
        partition.offset, partition.size,
    );
    exec::run(
        Command::new("qemu-img")
            .arg("convert")
            .args(COMPRESS)
            .args(["--image-opts", &image_opts, "-O", "qcow2"])
            .arg(out),
    )
    .with_context(|| {
        format!(
            "convert partition {number} of {} to {}",
            image.display(),
            out.display(),
        )
    })?;
    unbake(out)
}

/// Create a qcow2 image of `virtual_size` bytes with no backing file.
pub fn create_empty(out: &Path, virtual_size: u64) -> Result<()> {
    exec::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(out)
            .arg(virtual_size.to_string()),
    )
    .with_context(|| format!("create {}", out.display()))
}

/// Fail unless `blob` is a qcow2 image with no baked backing file.
pub fn ensure_unbaked(blob: &Path) -> Result<()> {
    let info = qcow2_info(blob)?;
    ensure!(
        info.backing_filename.is_none(),
        "{} has a baked backing file; the chain is supplied at launch",
        blob.display(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These exercise real `qemu-img`, which the dev shell provides. Skip
    /// rather than fail where it is absent, so the pure tests still run.
    fn have_qemu_img() -> bool {
        exec::have("qemu-img")
    }

    #[test]
    fn a_delta_ships_unbaked() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        create_empty(&base, 16 * 1024 * 1024).unwrap();

        let overlay = dir.path().join("overlay.qcow2");
        create_overlay(&overlay, &base).unwrap();
        // An overlay does have a backing file while it is being built on.
        assert!(qcow2_info(&overlay).unwrap().backing_filename.is_some());

        let delta = dir.path().join("delta.qcow2");
        finalize_delta(&overlay, &base, &delta).unwrap();
        ensure_unbaked(&delta).unwrap();
    }

    #[test]
    fn an_untouched_overlay_counts_as_unchanged() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        create_empty(&base, 16 * 1024 * 1024).unwrap();
        let overlay = dir.path().join("overlay.qcow2");
        create_overlay(&overlay, &base).unwrap();

        assert!(volume_unchanged(&overlay, &base).unwrap());
    }

    #[test]
    fn growing_an_overlay_makes_it_differ() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        create_empty(&base, 16 * 1024 * 1024).unwrap();
        let overlay = dir.path().join("overlay.qcow2");
        create_overlay(&overlay, &base).unwrap();
        grow_overlay(&overlay, 16 * 1024 * 1024).unwrap();

        // Differing virtual sizes settle it without a cluster comparison.
        assert!(!volume_unchanged(&overlay, &base).unwrap());
        assert_eq!(qcow2_info(&overlay).unwrap().virtual_size, 32 * 1024 * 1024);
    }

    #[test]
    fn relinking_leaves_the_originals_unbaked() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();

        // Two unbaked blobs, as they would come out of a layout.
        let blobs: Vec<PathBuf> = (0..2)
            .map(|i| {
                let path = dir.path().join(format!("blob{i}.qcow2"));
                create_empty(&path, 16 * 1024 * 1024).unwrap();
                path
            })
            .collect();

        let head = link_chain(&work, "disk", &blobs).unwrap();
        assert_ne!(head, blobs[1], "the head must be a copy, not the blob");
        assert!(qcow2_info(&head).unwrap().backing_filename.is_some());
        // The cache's copies are untouched, so another build can still use them.
        for blob in &blobs {
            ensure_unbaked(blob).unwrap();
        }
    }

    #[test]
    fn a_single_blob_chain_is_the_blob_itself() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob.qcow2");
        create_empty(&blob, 16 * 1024 * 1024).unwrap();
        let head = link_chain(dir.path(), "disk", std::slice::from_ref(&blob)).unwrap();
        assert_eq!(head, blob);
    }

    #[test]
    fn an_empty_chain_is_refused() {
        assert!(link_chain(Path::new("/nonexistent"), "disk", &[]).is_err());
    }
}
