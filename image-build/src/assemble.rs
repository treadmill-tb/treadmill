use std::path::PathBuf;

use anyhow::Context;

use treadmill_rs::image::assemble::ImageBuilder;
use treadmill_rs::image::parse::ImageMeta;

use crate::layer_arg::LayerArg;
use crate::layout::Layout;

/// What to assemble, and what to say about it.
///
/// Assembly starts an image from nothing: there is no lower to inherit from, so
/// a title is required and the layers given are the whole image.
#[derive(Debug, Clone)]
pub struct AssembleParams {
    /// Output OCI layout directory, holding `oci-layout`, `index.json` and
    /// `blobs/`. Created if absent.
    pub out: PathBuf,

    /// `org.opencontainers.image.title`.
    pub title: String,

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

    /// The layer blobs. The first blob of a role is its chain's base and each
    /// later one backs onto the one before it; only a base may be raw.
    pub layers: Vec<LayerArg>,
}

/// Assemble `args.layers` into a validated image layout at `args.out`.
pub fn assemble(args: &AssembleParams) -> anyhow::Result<()> {
    let layout = Layout::create(&args.out)?;

    let mut builder = ImageBuilder::new(ImageMeta {
        title: Some(args.title.clone()),
        version: args.version.clone(),
        description: args.description.clone(),
        created: args.created.clone(),
        revision: args.revision.clone(),
        documentation: args.documentation.clone(),
        base_name: None,
    });
    for layer in &args.layers {
        let blob = layout.store_layer(layer)?;
        builder
            .push(layer.role.clone(), blob)
            .with_context(|| format!("place {}", layer.path.display()))?;
    }
    let image = builder.build().context("assemble image")?;
    layout.write_image(&image)?;

    Ok(())
}
