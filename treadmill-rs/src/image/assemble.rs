//! Producing OCI image manifests for Treadmill images — the inverse of
//! [`super::parse`].
//!
//! Given an ordered list of layer blobs (with roles and sizes), build the OCI
//! [`ImageManifest`] that [`parse_image`](super::parse::parse_image) reads back.
//! Producer and consumer share this crate's media-type / annotation constants
//! and the same backing-chain wiring rules, so the two cannot drift — the
//! roundtrip tests below pin that.
//!
//! This is pure (no filesystem): the caller is responsible for hashing and
//! sizing the blobs and for writing the resulting manifest + blob store. The
//! `image-util assemble` command is the filesystem half.

use std::collections::HashMap;
use std::str::FromStr;

use oci_spec::image::{Descriptor, ImageManifest, ImageManifestBuilder, MediaType, SCHEMA_VERSION};

use super::annotations::{self, Role};
use super::digest::Digest;
use super::media_types;

/// `sha256("{}")` — the canonical empty-config blob, marking a pure artifact.
const EMPTY_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
/// Base64 of the empty-config blob `{}` (inlined via the descriptor `data`).
const EMPTY_CONFIG_DATA_B64: &str = "e30=";

/// One blob to place into the image, in chain order. The media type is derived
/// from the role ([`Role::Root`] → qcow2 disk, [`Role::Boot`] → boot FAT).
#[derive(Debug, Clone)]
pub struct LayerSpec {
    pub digest: Digest,
    pub size: u64,
    pub role: Role,
    /// qcow2 virtual size in bytes. Emitted for root layers; `None` for boot.
    pub virtual_size: Option<u64>,
}

/// Image-level metadata carried on the manifest annotations.
#[derive(Debug, Clone, Default)]
pub struct ImageMeta {
    pub title: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// Why [`build_manifest`] could not assemble a manifest.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AssembleError {
    /// No `role = Root` layer was supplied; an image needs at least one (it
    /// names the head of the backing chain).
    NoRootLayer,
}

impl std::fmt::Display for AssembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssembleError::NoRootLayer => write!(f, "image has no role=root layer"),
        }
    }
}

impl std::error::Error for AssembleError {}

fn media_type_for(role: Role) -> MediaType {
    MediaType::Other(
        match role {
            Role::Root => media_types::DISK_QCOW2,
            Role::Boot => media_types::BOOT_FAT_V1,
        }
        .to_string(),
    )
}

/// Convert a Treadmill [`Digest`] to the `oci_spec` digest type. Infallible: a
/// `Digest` always renders as a valid `sha256:<hex>` OCI digest.
fn oci_digest(d: &Digest) -> oci_spec::image::Digest {
    oci_spec::image::Digest::from_str(&d.encoded())
        .expect("a Treadmill Digest always encodes to a valid OCI digest")
}

/// Build the OCI image manifest for an ordered list of layers.
///
/// Chain wiring mirrors [`parse_image`](super::parse::parse_image): among the
/// `role = Root` layers, in order, the first gets no `lower`, each later one's
/// `lower` is the previous root's digest, and the manifest `head` is the last
/// root. `role = Boot` layers are standalone (no `lower`, never `head`).
pub fn build_manifest(
    layers: &[LayerSpec],
    meta: &ImageMeta,
) -> Result<ImageManifest, AssembleError> {
    let mut descriptors = Vec::with_capacity(layers.len());
    let mut prev_root: Option<Digest> = None;
    let mut head: Option<Digest> = None;

    for layer in layers {
        let mut ann: HashMap<String, String> = HashMap::new();
        ann.insert(
            annotations::ROLE.to_string(),
            layer.role.as_str().to_string(),
        );

        if layer.role == Role::Root {
            if let Some(vs) = layer.virtual_size {
                ann.insert(annotations::QCOW2_VIRTUAL_SIZE.to_string(), vs.to_string());
            }
            if let Some(prev) = prev_root {
                ann.insert(annotations::QCOW2_LOWER.to_string(), prev.encoded());
            }
            prev_root = Some(layer.digest);
            head = Some(layer.digest);
        }

        let mut desc = Descriptor::new(
            media_type_for(layer.role),
            layer.size,
            oci_digest(&layer.digest),
        );
        desc.set_annotations(Some(ann));
        descriptors.push(desc);
    }

    let head = head.ok_or(AssembleError::NoRootLayer)?;

    // Empty config marks the manifest as a pure artifact (matches `parse`).
    let mut config = Descriptor::new(
        MediaType::EmptyJSON,
        2,
        oci_spec::image::Digest::from_str(EMPTY_CONFIG_DIGEST)
            .expect("EMPTY_CONFIG_DIGEST is a valid OCI digest"),
    );
    config.set_data(Some(EMPTY_CONFIG_DATA_B64.to_string()));

    let mut manifest_ann: HashMap<String, String> = HashMap::new();
    manifest_ann.insert(annotations::QCOW2_HEAD.to_string(), head.encoded());
    if let Some(t) = &meta.title {
        manifest_ann.insert(annotations::oci::TITLE.to_string(), t.clone());
    }
    if let Some(v) = &meta.version {
        manifest_ann.insert(annotations::oci::VERSION.to_string(), v.clone());
    }
    if let Some(d) = &meta.description {
        manifest_ann.insert(annotations::oci::DESCRIPTION.to_string(), d.clone());
    }

    Ok(ImageManifestBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageManifest)
        .artifact_type(MediaType::Other(
            media_types::IMAGE_ARTIFACT_TYPE.to_string(),
        ))
        .config(config)
        .layers(descriptors)
        .annotations(manifest_ann)
        .build()
        .expect("the image manifest builder has all required fields set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::parse::parse_image;

    fn dg(byte: u8) -> Digest {
        Digest::from_sha256([byte; 32])
    }

    #[test]
    fn roundtrips_a_three_root_chain() {
        let layers = vec![
            LayerSpec {
                digest: dg(1),
                size: 100,
                role: Role::Root,
                virtual_size: Some(1000),
            },
            LayerSpec {
                digest: dg(2),
                size: 50,
                role: Role::Root,
                virtual_size: Some(2000),
            },
            LayerSpec {
                digest: dg(3),
                size: 25,
                role: Role::Root,
                virtual_size: Some(3000),
            },
        ];
        let meta = ImageMeta {
            title: Some("Triple".to_string()),
            version: Some("1.2.3".to_string()),
            ..Default::default()
        };

        let img = parse_image(&build_manifest(&layers, &meta).unwrap()).unwrap();

        assert_eq!(img.title.as_deref(), Some("Triple"));
        assert_eq!(img.layers.len(), 3);
        assert_eq!(img.head, dg(3));
        assert_eq!(img.layers[0].lower, None);
        assert_eq!(img.layers[1].lower, Some(dg(1)));
        assert_eq!(img.layers[2].lower, Some(dg(2)));
        assert_eq!(img.layers[0].virtual_size, Some(1000));
        assert_eq!(img.layers[2].virtual_size, Some(3000));
        assert_eq!(img.layers[0].media_type, media_types::DISK_QCOW2);
    }

    #[test]
    fn roundtrips_a_boot_plus_root_image() {
        let layers = vec![
            LayerSpec {
                digest: dg(9),
                size: 10,
                role: Role::Boot,
                virtual_size: None,
            },
            LayerSpec {
                digest: dg(1),
                size: 100,
                role: Role::Root,
                virtual_size: Some(2048),
            },
        ];

        let img = parse_image(&build_manifest(&layers, &ImageMeta::default()).unwrap()).unwrap();

        assert_eq!(img.layers.len(), 2);
        let boot = &img.layers[0];
        assert_eq!(boot.role, Some(Role::Boot));
        assert_eq!(boot.lower, None);
        assert_eq!(boot.virtual_size, None);
        assert_eq!(boot.media_type, media_types::BOOT_FAT_V1);
        // The boot layer is standalone — never the head.
        assert_ne!(img.head, boot.digest);
        assert_eq!(img.head, dg(1));
    }

    #[test]
    fn rejects_an_image_with_no_root_layer() {
        let layers = vec![LayerSpec {
            digest: dg(9),
            size: 10,
            role: Role::Boot,
            virtual_size: None,
        }];
        assert!(matches!(
            build_manifest(&layers, &ImageMeta::default()),
            Err(AssembleError::NoRootLayer)
        ));
    }
}
