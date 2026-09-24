use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail, ensure};
use clap::Parser;

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::parse::LayerFormat;

use crate::layer_arg::ChainArg;
use crate::layout::{Layout, qcow2_info, read_image, verify_blob_digest};

const DEFAULT_MAX_HEAD_VIRTUAL_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Validate a built Treadmill OCI image layout.
#[derive(Debug, Parser)]
pub struct VerifyArgs {
    /// Path to the OCI image layout directory.
    pub layout: PathBuf,

    /// An expected chain as `ROLE=LAYERS`. Repeatable; when given, the image
    /// must provide exactly these roles, each with a chain of that many layers.
    #[arg(long = "chain", value_name = "ROLE=LAYERS")]
    chains: Vec<ChainArg>,

    /// Expected `org.opencontainers.image.title`; asserted only when given.
    #[arg(long)]
    title: Option<String>,

    /// Ceiling for the virtual size of every chain's head, in bytes. Must not
    /// exceed the smallest working disk of any supervisor the image may be
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
    // Parsing validates the image's structure; what is left to check here is
    // what needs the blobs.
    let image = read_image(&layout)?;

    if let Some(title) = &args.title {
        ensure!(
            image.meta.title.as_deref() == Some(title.as_str()),
            "unexpected image title: got {:?}, expected {title:?}",
            image.meta.title,
        );
    }

    if !args.chains.is_empty() {
        let mut expected: BTreeMap<&Role, usize> = BTreeMap::new();
        for chain in &args.chains {
            ensure!(
                expected.insert(&chain.role, chain.layers).is_none(),
                "--chain {} is given more than once",
                chain.role,
            );
        }
        let actual: BTreeMap<&Role, usize> = image
            .chains()
            .map(|chain| (chain.role(), chain.layers().len()))
            .collect();
        ensure!(
            actual == expected,
            "unexpected chains: got {}, expected {}",
            describe(&actual),
            describe(&expected),
        );
    }

    for layer in image.layers() {
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

        match &layer.format {
            LayerFormat::Qcow2 { virtual_size, .. } => {
                let info = qcow2_info(&path)?;
                ensure!(
                    info.virtual_size == *virtual_size,
                    "layer {} advertises virtual size {virtual_size} but qemu-img reports {}",
                    layer.digest,
                    info.virtual_size,
                );
                ensure!(
                    info.backing_filename.is_none(),
                    "layer {} has a baked backing_file; the chain is supplied at launch",
                    layer.digest,
                );
            }
            LayerFormat::Raw => {}
            LayerFormat::Unknown { media_type } => bail!(
                "layer {} has media type {media_type}, which image-util cannot verify",
                layer.digest,
            ),
        }
    }

    if args.max_head_virtual_size > 0 {
        for chain in image.chains() {
            let virtual_size = chain
                .virtual_size()
                .expect("every layer of a known format has a virtual size");
            ensure!(
                virtual_size <= args.max_head_virtual_size,
                "role {}'s virtual size {virtual_size} exceeds the {} byte ceiling; a \
                 supervisor whose working disk is smaller rejects the image at job launch",
                chain.role(),
                args.max_head_virtual_size,
            );
        }
    }

    Ok(())
}

fn describe(chains: &BTreeMap<&Role, usize>) -> String {
    let chains: Vec<String> = chains
        .iter()
        .map(|(role, layers)| format!("{role}={layers}"))
        .collect();
    format!("[{}]", chains.join(", "))
}
