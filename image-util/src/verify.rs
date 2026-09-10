use std::fs;
use std::path::PathBuf;

use anyhow::{Context, ensure};
use clap::Parser;

use crate::chain;
use crate::layout::{Layout, is_fat_image, qcow2_header, read_image, verify_blob_digest};

const DEFAULT_MAX_HEAD_VIRTUAL_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Validate a built Treadmill OCI image layout.
#[derive(Debug, Parser)]
pub struct VerifyArgs {
    /// Path to the OCI image layout directory.
    pub layout: PathBuf,

    /// Expected number of `role=root` layers; asserted only when given.
    #[arg(long)]
    root_layers: Option<usize>,

    /// Expected number of `role=boot` layers; asserted only when given.
    #[arg(long)]
    boot_layers: Option<usize>,

    /// Expected `org.opencontainers.image.title`; asserted only when given.
    #[arg(long)]
    title: Option<String>,

    /// Ceiling for the head layer's virtual size, in bytes. Must not exceed the
    /// smallest `working_disk_max_bytes` of any supervisor the image may be
    /// scheduled on. Pass 0 to disable the check.
    #[arg(long, default_value_t = DEFAULT_MAX_HEAD_VIRTUAL_SIZE)]
    max_head_virtual_size: u64,

    /// Human-readable label used in messages (defaults to the layout path).
    #[arg(long)]
    pub name: Option<String>,
}

impl VerifyArgs {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.layout.display().to_string())
    }
}

pub fn verify(args: &VerifyArgs) -> anyhow::Result<()> {
    let layout = Layout::open(&args.layout)?;
    let image = read_image(&layout)?;
    let split = chain::split_and_check_order(&image)?;

    if let Some(title) = &args.title {
        ensure!(
            image.title.as_deref() == Some(title.as_str()),
            "unexpected image title: got {:?}, expected {title:?}",
            image.title,
        );
    }
    if let Some(expected) = args.root_layers {
        ensure!(
            split.roots.len() == expected,
            "wrong root-layer count: got {}, expected {expected}",
            split.roots.len(),
        );
    }
    if let Some(expected) = args.boot_layers {
        ensure!(
            split.boots.len() == expected,
            "wrong boot-layer count: got {}, expected {expected}",
            split.boots.len(),
        );
    }
    ensure!(
        split.boots.len() <= 1,
        "an image carries at most one role=boot layer, found {}",
        split.boots.len(),
    );

    for layer in &image.layers {
        let path = layout.blob_path(&layer.digest);
        let meta = fs::metadata(&path)
            .with_context(|| format!("layer blob {} is missing", path.display()))?;
        ensure!(
            meta.len() == layer.size,
            "layer blob {} size {} disagrees with its descriptor ({})",
            path.display(),
            meta.len(),
            layer.size,
        );
        verify_blob_digest(&path, &layer.digest)?;
    }

    let mut previous_virtual_size = 0u64;
    for (i, layer) in split.roots.iter().enumerate() {
        let path = layout.blob_path(&layer.digest);
        let header = qcow2_header(&path)?;

        let annotated = layer.virtual_size.with_context(|| {
            format!(
                "root layer {i} ({}) carries no dev.treadmill.qcow2.virtual-size",
                layer.digest,
            )
        })?;
        ensure!(
            header.virtual_size == annotated,
            "root layer {i} ({}) advertises virtual size {annotated} but its qcow2 header says {}",
            layer.digest,
            header.virtual_size,
        );
        ensure!(
            !header.has_backing_file,
            "root layer {i} ({}) has a baked backing_file; the chain is supplied at launch",
            layer.digest,
        );
        ensure!(
            annotated >= previous_virtual_size,
            "root layer {i} ({}) virtual size {annotated} is smaller than its lower's \
             ({previous_virtual_size}), which would truncate the chain",
            layer.digest,
        );
        previous_virtual_size = annotated;
    }

    for layer in &split.boots {
        let path = layout.blob_path(&layer.digest);
        ensure!(
            is_fat_image(&path)?,
            "boot layer {} is not a FAT filesystem image",
            layer.digest,
        );
    }

    let (walked, head_virtual_size) = image
        .backing_chain()
        .map_err(|e| anyhow::anyhow!("backing chain does not walk: {e}"))?;
    ensure!(
        walked
            .iter()
            .map(|l| l.digest)
            .eq(split.roots.iter().map(|l| l.digest)),
        "the walked backing chain does not match the role=root layers in array order",
    );

    if args.max_head_virtual_size > 0 {
        ensure!(
            head_virtual_size <= args.max_head_virtual_size,
            "head virtual size {head_virtual_size} exceeds the {} byte ceiling; a supervisor \
             whose working_disk_max_bytes is smaller rejects the image at job launch",
            args.max_head_virtual_size,
        );
    }

    Ok(())
}
