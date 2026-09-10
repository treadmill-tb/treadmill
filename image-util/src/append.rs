use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::assemble::{self, ImageMeta, LayerSpec};

use crate::chain;
use crate::layer_arg::LayerArg;
use crate::layout::{Layout, qcow2_header, read_image};

/// Append layers on top of an existing image layout's backing chain.
#[derive(Debug, Parser)]
pub struct AppendArgs {
    /// Lower OCI image layout whose layers are carried through by digest.
    #[arg(long)]
    lower: PathBuf,

    /// Output OCI layout directory (may be the lower layout, appended in place).
    #[arg(short = 'o', long)]
    out: PathBuf,

    /// A layer blob as `ROLE=PATH` (ROLE is `root` or `boot`). Repeatable, and
    /// ORDER IS SIGNIFICANT — it extends the backing chain.
    #[arg(long = "layer", value_name = "ROLE=PATH", required = true)]
    layers: Vec<LayerArg>,

    /// `org.opencontainers.image.title`; inherited from the lower if omitted.
    #[arg(long)]
    title: Option<String>,

    /// `org.opencontainers.image.version`; inherited from the lower if omitted.
    #[arg(long)]
    version: Option<String>,

    /// `org.opencontainers.image.description`; inherited from the lower if
    /// omitted.
    #[arg(long)]
    description: Option<String>,

    /// `org.opencontainers.image.base.name`; defaults to the lower layout's
    /// manifest digest.
    #[arg(long)]
    base_name: Option<String>,

    /// Human-readable label used in messages (defaults to the title).
    #[arg(long)]
    pub name: Option<String>,
}

impl AppendArgs {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| self.out.display().to_string())
    }
}

pub fn append(args: &AppendArgs) -> anyhow::Result<()> {
    let lower = Layout::open(&args.lower)?;
    let (lower_manifest_digest, _) = lower.manifest()?;
    let lower_image = read_image(&lower)?;
    chain::split_and_check_order(&lower_image)
        .context("the lower layout's layer order disagrees with its backing chain")?;

    let mut specs: Vec<LayerSpec> =
        Vec::with_capacity(lower_image.layers.len() + args.layers.len());
    for layer in &lower_image.layers {
        specs.push(LayerSpec {
            digest: layer.digest,
            size: layer.size,
            role: layer
                .role
                .with_context(|| format!("lower layer {} carries no role", layer.digest))?,
            virtual_size: layer.virtual_size,
        });
    }

    let out = Layout::create(&args.out)?;
    for layer in &lower_image.layers {
        out.store_blob_from(&lower, &layer.digest)
            .with_context(|| format!("carry lower layer {} through", layer.digest))?;
    }

    for layer in &args.layers {
        specs.push(store_layer(&out, layer.role, &layer.path)?);
    }

    let meta = ImageMeta {
        title: args.title.clone().or_else(|| lower_image.title.clone()),
        version: args.version.clone().or_else(|| lower_image.version.clone()),
        description: args
            .description
            .clone()
            .or_else(|| lower_image.description.clone()),
        base_name: Some(
            args.base_name
                .clone()
                .unwrap_or_else(|| lower_manifest_digest.encoded()),
        ),
    };

    let manifest = assemble::build_manifest(&specs, &meta).context("assemble manifest")?;
    out.write_manifest(&manifest)?;
    out.prune()?;

    Ok(())
}

pub fn store_layer(layout: &Layout, role: Role, path: &Path) -> anyhow::Result<LayerSpec> {
    let (digest, size) = layout
        .store_file(path)
        .with_context(|| format!("store layer blob {}", path.display()))?;
    let virtual_size = match role {
        Role::Root => Some(
            qcow2_header(path)
                .with_context(|| format!("read qcow2 virtual size of {}", path.display()))?
                .virtual_size,
        ),
        Role::Boot => None,
    };
    Ok(LayerSpec {
        digest,
        size,
        role,
        virtual_size,
    })
}
