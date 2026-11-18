use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail, ensure};

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::parse::LayerFormat;

use crate::layer_arg::ChainArg;
use crate::layout::{Layout, qcow2_info, read_image, verify_blob_digest};

/// The default ceiling on a chain head's virtual size: 10 GiB.
///
/// A supervisor refuses an image whose head does not fit its working disk, so
/// this has to stay at or below the smallest such disk in the fleet.
pub const DEFAULT_MAX_HEAD_VIRTUAL_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// What to check a built layout against.
///
/// Parsing the manifest already validates the image's structure, so these are
/// the expectations that need the blobs, or that only the caller knows.
#[derive(Debug, Clone)]
pub struct VerifyParams {
    /// Path to the OCI image layout directory.
    pub layout: PathBuf,

    /// The chains the image must provide, each with its expected length. When
    /// non-empty the match is exact: a role the image has and this does not is
    /// as much a failure as one that is missing.
    pub chains: Vec<ChainArg>,

    /// Expected `org.opencontainers.image.title`; asserted only when given.
    pub title: Option<String>,

    /// Ceiling for the virtual size of every chain's head, in bytes. 0
    /// disables the check. See [`DEFAULT_MAX_HEAD_VIRTUAL_SIZE`].
    pub max_head_virtual_size: u64,
}

impl VerifyParams {
    /// Check `layout`, with every expectation left at its default.
    pub fn new(layout: impl Into<PathBuf>) -> Self {
        Self {
            layout: layout.into(),
            chains: Vec::new(),
            title: None,
            max_head_virtual_size: DEFAULT_MAX_HEAD_VIRTUAL_SIZE,
        }
    }
}

/// Check that the layout at `args.layout` is a well-formed Treadmill image and
/// meets every expectation `args` states.
pub fn verify(args: &VerifyParams) -> anyhow::Result<()> {
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
                "layer {} has media type {media_type}, which cannot be verified",
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
