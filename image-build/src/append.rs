use std::path::PathBuf;

use anyhow::Context;

use treadmill_rs::image::assemble::ImageBuilder;

use crate::layer_arg::LayerArg;
use crate::layout::{Layout, read_image};

/// What to append, and onto what.
///
/// Every metadata field is an override: left `None`, the lower image's value
/// is carried through, so a derived image keeps its base's description unless
/// it says otherwise.
#[derive(Debug, Clone)]
pub struct AppendParams {
    /// Lower OCI image layout whose layers are carried through by digest.
    pub lower: PathBuf,

    /// Output OCI layout directory. May be the lower layout, which appends in
    /// place; [`Layout::prune`] is what then drops the superseded manifest.
    pub out: PathBuf,

    /// Layer blobs to place, each on top of its role's chain — which only a
    /// qcow2 blob can go on — or starting that chain if the lower has no such
    /// role. Each later blob of a role backs onto the one before it. Empty
    /// carries the lower's layers through under the new metadata.
    pub layers: Vec<LayerArg>,

    /// `org.opencontainers.image.title`.
    pub title: Option<String>,

    /// `org.opencontainers.image.version`.
    pub version: Option<String>,

    /// `org.opencontainers.image.description`.
    pub description: Option<String>,

    /// `org.opencontainers.image.created`, an RFC 3339 timestamp.
    pub created: Option<String>,

    /// `org.opencontainers.image.revision`, the source commit.
    pub revision: Option<String>,

    /// `org.opencontainers.image.documentation`, a documentation URL.
    pub documentation: Option<String>,

    /// `org.opencontainers.image.base.name`. Unlike the fields above this is
    /// not inherited: left `None` it becomes the lower layout's own manifest
    /// digest, which is what actually identifies what this was built on.
    pub base_name: Option<String>,
}

/// Append `params.layers` onto `params.lower`, writing the result to
/// `params.out`.
pub fn append(args: &AppendParams) -> anyhow::Result<()> {
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
