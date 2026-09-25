use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use treadmill_rs::image::assemble::ImageBuilder;

use crate::layer_arg::LayerArg;
use crate::layout::{Layout, read_image};

/// Append layers on top of the chains of an existing image layout.
#[derive(Debug, Parser)]
pub struct AppendArgs {
    /// Lower OCI image layout whose layers are carried through by digest.
    #[arg(long)]
    lower: PathBuf,

    /// Output OCI layout directory (may be the lower layout, appended in place).
    #[arg(short = 'o', long)]
    out: PathBuf,

    /// A layer blob as `ROLE=FORMAT:PATH`, where FORMAT is `qcow2` or `raw`,
    /// placed on top of ROLE's chain (which only a qcow2 blob can go on), or
    /// starting it if the lower has no such role.
    /// Repeatable: each later blob of a role backs onto the one before it.
    /// Without any, the lower's layers are carried through under the new
    /// metadata.
    #[arg(long = "layer", value_name = "ROLE=FORMAT:PATH")]
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

    /// `org.opencontainers.image.created` (RFC 3339 timestamp).
    /// Inherited from the lower image on append if omitted.
    #[arg(long)]
    created: Option<String>,

    /// `org.opencontainers.image.revision` (source commit).
    /// Inherited from the lower image on append if omitted.
    #[arg(long)]
    revision: Option<String>,

    /// `org.opencontainers.image.documentation` (documentation URL).
    /// Inherited from the lower image on append if omitted.
    #[arg(long)]
    documentation: Option<String>,

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

    let out = Layout::create(&args.out)?;
    for layer in lower_image.layers() {
        out.store_blob_from(&lower, &layer.digest)
            .with_context(|| format!("carry lower layer {} through", layer.digest))?;
    }

    let mut builder = ImageBuilder::from_image(lower_image);
    for layer in &args.layers {
        let blob = out.store_layer(layer)?;
        builder
            .push(layer.role.clone(), blob)
            .with_context(|| format!("place {}", layer.path.display()))?;
    }

    let meta = &mut builder.meta;
    if let Some(title) = &args.title {
        meta.title = Some(title.clone());
    }
    if let Some(version) = &args.version {
        meta.version = Some(version.clone());
    }
    if let Some(description) = &args.description {
        meta.description = Some(description.clone());
    }
    if let Some(created) = &args.created {
        meta.created = Some(created.clone());
    }
    if let Some(revision) = &args.revision {
        meta.revision = Some(revision.clone());
    }
    if let Some(documentation) = &args.documentation {
        meta.documentation = Some(documentation.clone());
    }
    meta.base_name = Some(
        args.base_name
            .clone()
            .unwrap_or_else(|| lower_manifest_digest.encoded()),
    );

    let image = builder.build().context("assemble image")?;
    out.write_image(&image)?;
    out.prune()?;

    Ok(())
}
