//! Producing Treadmill images and their OCI manifests — the inverse of
//! [`super::parse`].
//!
//! [`ImageBuilder`] grows an image one blob at a time, each placed on top of a
//! role's chain, and validates the result as a [`TreadmillImage`].
//! [`TreadmillImage::to_manifest`] then renders the OCI [`ImageManifest`] that
//! [`parse_image`](super::parse::parse_image) reads back, and
//! [`manifest_bytes`] its canonical (RFC 8785) serialization. Producer and consumer share
//! this crate's media-type / annotation constants and the one validation in
//! [`TreadmillImage::new`], so the two cannot drift — the roundtrip tests
//! below pin that.
//!
//! This is pure (no filesystem): the caller is responsible for hashing and
//! sizing the blobs and for writing the resulting manifest + blob store. The
//! `image-util` commands are the filesystem half.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use oci_spec::image::{Descriptor, ImageManifest, ImageManifestBuilder, MediaType, SCHEMA_VERSION};

use super::annotations::{self, Role};
use super::digest::Digest;
use super::media_types;
use super::parse::{ImageError, ImageLayer, ImageMeta, LayerFormat, TreadmillImage};

/// `sha256("{}")` — the canonical empty-config blob, marking a pure artifact.
pub const EMPTY_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
/// The empty-config blob itself.
pub const EMPTY_CONFIG: &[u8] = b"{}";
/// Base64 of the empty-config blob `{}` (inlined via the descriptor `data`).
const EMPTY_CONFIG_DATA_B64: &str = "e30=";

/// A blob to place into an image.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Blob {
    pub digest: Digest,
    pub size: u64,
    pub format: BlobFormat,
}

/// The format of a [`Blob`]. How it links to other layers is up to the
/// [`ImageBuilder`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlobFormat {
    Qcow2 { virtual_size: u64 },
    Raw,
}

/// Why [`ImageBuilder::push`] could not place a blob.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PushError {
    /// Only a qcow2 blob can go on top of an existing chain.
    CannotBackOnto { role: Role, digest: Digest },
    /// The role's current head is of a format a qcow2 layer cannot back onto.
    UnbackableHead { role: Role, head: Digest },
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushError::CannotBackOnto { role, digest } => write!(
                f,
                "blob {digest} is not qcow2, so it cannot go on top of role {role}'s chain",
            ),
            PushError::UnbackableHead { role, head } => write!(
                f,
                "role {role}'s head {head} is of a format a qcow2 layer cannot back onto",
            ),
        }
    }
}

impl std::error::Error for PushError {}

/// Grows an image one blob at a time.
#[derive(Debug, Clone, Default)]
pub struct ImageBuilder {
    layers: Vec<ImageLayer>,
    pub meta: ImageMeta,
}

impl ImageBuilder {
    pub fn new(meta: ImageMeta) -> Self {
        ImageBuilder {
            layers: Vec::new(),
            meta,
        }
    }

    /// Start from `image`, carrying all its layers (and its metadata) through.
    pub fn from_image(image: TreadmillImage) -> Self {
        let (layers, meta) = image.into_parts();
        ImageBuilder { layers, meta }
    }

    /// Place `blob` on top of `role`'s chain, making it the new head, or start
    /// the chain with it if the role is new. The previous head keeps its digest
    /// and loses the role.
    pub fn push(&mut self, role: Role, blob: Blob) -> Result<&mut Self, PushError> {
        let head = self
            .layers
            .iter_mut()
            .find(|layer| layer.role.as_ref() == Some(&role));

        let lower = match head {
            None => None,
            Some(head) => {
                if !matches!(blob.format, BlobFormat::Qcow2 { .. }) {
                    return Err(PushError::CannotBackOnto {
                        role,
                        digest: blob.digest,
                    });
                }
                if !head.format.can_back_qcow2() {
                    return Err(PushError::UnbackableHead {
                        role,
                        head: head.digest,
                    });
                }
                head.role = None;
                Some(head.digest)
            }
        };

        let format = match blob.format {
            BlobFormat::Qcow2 { virtual_size } => LayerFormat::Qcow2 {
                virtual_size,
                lower,
            },
            BlobFormat::Raw => LayerFormat::Raw,
        };
        self.layers.push(ImageLayer {
            digest: blob.digest,
            size: blob.size,
            format,
            role: Some(role),
        });
        Ok(self)
    }

    pub fn build(self) -> Result<TreadmillImage, ImageError> {
        TreadmillImage::new(self.layers, self.meta)
    }
}

/// Convert a Treadmill [`Digest`] to the `oci_spec` digest type. Infallible: a
/// `Digest` always renders as a valid `sha256:<hex>` OCI digest.
fn oci_digest(d: &Digest) -> oci_spec::image::Digest {
    oci_spec::image::Digest::from_str(&d.encoded())
        .expect("a Treadmill Digest always encodes to a valid OCI digest")
}

impl TreadmillImage {
    /// The OCI image manifest of this image, with its layers in canonical
    /// order.
    pub fn to_manifest(&self) -> ImageManifest {
        let descriptors = self
            .layers()
            .iter()
            .map(|layer| {
                let mut ann: HashMap<String, String> = HashMap::new();
                if let Some(role) = &layer.role {
                    ann.insert(annotations::ROLE.to_string(), role.to_string());
                }
                match &layer.format {
                    LayerFormat::Qcow2 {
                        virtual_size,
                        lower,
                    } => {
                        ann.insert(
                            annotations::QCOW2_VIRTUAL_SIZE.to_string(),
                            virtual_size.to_string(),
                        );
                        if let Some(lower) = lower {
                            ann.insert(annotations::QCOW2_LOWER.to_string(), lower.encoded());
                        }
                    }
                    LayerFormat::Raw | LayerFormat::Unknown { .. } => {}
                }

                let mut desc = Descriptor::new(
                    MediaType::from(layer.format.media_type()),
                    layer.size,
                    oci_digest(&layer.digest),
                );
                if !ann.is_empty() {
                    desc.set_annotations(Some(ann));
                }
                desc
            })
            .collect::<Vec<_>>();

        // Empty config marks the manifest as a pure artifact (matches `parse`).
        let mut config = Descriptor::new(
            MediaType::EmptyJSON,
            EMPTY_CONFIG.len() as u64,
            oci_spec::image::Digest::from_str(EMPTY_CONFIG_DIGEST)
                .expect("EMPTY_CONFIG_DIGEST is a valid OCI digest"),
        );
        config.set_data(Some(EMPTY_CONFIG_DATA_B64.to_string()));

        let meta = &self.meta;
        let manifest_ann: HashMap<String, String> = [
            (annotations::oci::TITLE, &meta.title),
            (annotations::oci::VERSION, &meta.version),
            (annotations::oci::DESCRIPTION, &meta.description),
            (annotations::oci::CREATED, &meta.created),
            (annotations::oci::REVISION, &meta.revision),
            (annotations::oci::DOCUMENTATION, &meta.documentation),
            (annotations::oci::BASE_NAME, &meta.base_name),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.clone().map(|value| (key.to_string(), value)))
        .collect();

        let mut manifest = ImageManifestBuilder::default()
            .schema_version(SCHEMA_VERSION)
            .media_type(MediaType::ImageManifest)
            .artifact_type(MediaType::Other(
                media_types::IMAGE_ARTIFACT_TYPE.to_string(),
            ))
            .config(config)
            .layers(descriptors)
            .build()
            .expect("the image manifest builder has all required fields set");
        manifest.set_annotations((!manifest_ann.is_empty()).then_some(manifest_ann));
        manifest
    }
}

/// The canonical serialization of `manifest`, whose digest is the manifest
/// digest.
///
/// The digest is taken over the manifest's bytes, so two writers producing the
/// same image must produce the same bytes. Plain `serde_json` writes a
/// `HashMap`'s keys in an order that differs between processes; the JSON
/// Canonicalization Scheme (RFC 8785) sorts them and drops insignificant
/// whitespace.
pub fn manifest_bytes(manifest: &ImageManifest) -> Vec<u8> {
    serde_json_canonicalizer::to_vec(manifest).expect("an OCI image manifest serializes to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::parse::parse_image;

    fn dg(byte: u8) -> Digest {
        Digest::from_sha256([byte; 32])
    }

    fn role(name: &str) -> Role {
        name.parse().unwrap()
    }

    fn qcow2(byte: u8, virtual_size: u64) -> Blob {
        Blob {
            digest: dg(byte),
            size: 10 * u64::from(byte),
            format: BlobFormat::Qcow2 { virtual_size },
        }
    }

    fn netboot() -> TreadmillImage {
        let mut builder = ImageBuilder::new(ImageMeta {
            title: Some("Netboot".to_string()),
            version: Some("13".to_string()),
            created: Some("2026-09-25T14:30:12Z".to_string()),
            revision: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            documentation: Some(
                "https://github.com/example/images/blob/0123456/README.md".to_string(),
            ),
            base_name: Some("ghcr.io/example/base@sha256:abc".to_string()),
            ..Default::default()
        });
        builder
            .push(role("rootfs"), qcow2(1, 1000))
            .unwrap()
            .push(role("bootfs"), qcow2(9, 512))
            .unwrap()
            .push(role("rootfs"), qcow2(2, 2000))
            .unwrap()
            .push(role("rootfs"), qcow2(3, 3000))
            .unwrap();
        builder.build().unwrap()
    }

    #[test]
    fn pushing_extends_a_role_and_moves_the_role_to_the_new_head() {
        let img = netboot();

        let rootfs = img.chain("rootfs").unwrap();
        assert_eq!(
            rootfs.layers().iter().map(|l| l.digest).collect::<Vec<_>>(),
            [dg(1), dg(2), dg(3)],
        );
        assert_eq!(rootfs.head().digest, dg(3));
        assert_eq!(rootfs.layers()[0].role, None);
        assert_eq!(rootfs.layers()[1].role, None);
        assert_eq!(rootfs.layers()[1].lower(), Some(&dg(1)));

        let bootfs = img.chain("bootfs").unwrap();
        assert_eq!(bootfs.layers().len(), 1);
        assert_eq!(bootfs.head().lower(), None);
    }

    #[test]
    fn a_manifest_roundtrips_through_parse() {
        let img = netboot();
        let manifest = img.to_manifest();

        assert_eq!(parse_image(&manifest).unwrap(), img);

        // Layers are emitted in canonical order: chains sorted by role, base
        // first.
        let digests: Vec<String> = manifest
            .layers()
            .iter()
            .map(|d| d.digest().to_string())
            .collect();
        assert_eq!(digests, [dg(9), dg(1), dg(2), dg(3)].map(|d| d.encoded()));
    }

    #[test]
    fn appending_to_a_parsed_image_roundtrips() {
        let mut builder = ImageBuilder::from_image(netboot());
        builder.meta.title = Some("Derived".to_string());
        builder.push(role("bootfs"), qcow2(10, 512)).unwrap();
        let img = builder.build().unwrap();

        assert_eq!(parse_image(&img.to_manifest()).unwrap(), img);
        assert_eq!(img.meta.version.as_deref(), Some("13"));
        let bootfs = img.chain("bootfs").unwrap();
        assert_eq!(
            bootfs.layers().iter().map(|l| l.digest).collect::<Vec<_>>(),
            [dg(9), dg(10)],
        );
    }

    #[test]
    fn the_canonical_manifest_matches_the_documented_shape() {
        let mut builder = ImageBuilder::new(ImageMeta {
            title: Some("t".to_string()),
            ..Default::default()
        });
        builder
            .push(
                role("disk"),
                Blob {
                    digest: dg(1),
                    size: 4,
                    format: BlobFormat::Raw,
                },
            )
            .unwrap()
            .push(role("disk"), qcow2(2, 8))
            .unwrap();
        let bytes = manifest_bytes(&builder.build().unwrap().to_manifest());

        let expected = format!(
            concat!(
                r#"{{"annotations":{{"org.opencontainers.image.title":"t"}},"#,
                r#""artifactType":"application/vnd.treadmill.image.v2+json","#,
                r#""config":{{"data":"e30=","digest":"{config}","mediaType":"application/vnd.oci.empty.v1+json","size":2}},"#,
                r#""layers":["#,
                r#"{{"digest":"{d1}","mediaType":"application/vnd.treadmill.raw","size":4}},"#,
                r#"{{"annotations":{{"dev.treadmill.qcow2.lower":"{d1}","dev.treadmill.qcow2.virtual-size":"8","dev.treadmill.role":"disk"}},"#,
                r#""digest":"{d2}","mediaType":"application/vnd.treadmill.qcow2","size":20}}"#,
                r#"],"#,
                r#""mediaType":"application/vnd.oci.image.manifest.v1+json","schemaVersion":2}}"#,
            ),
            config = EMPTY_CONFIG_DIGEST,
            d1 = dg(1).encoded(),
            d2 = dg(2).encoded(),
        );
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn a_raw_blob_cannot_go_on_top_of_a_chain() {
        let mut builder = ImageBuilder::default();
        builder.push(role("disk"), qcow2(1, 8)).unwrap();
        assert_eq!(
            builder
                .push(
                    role("disk"),
                    Blob {
                        digest: dg(2),
                        size: 8,
                        format: BlobFormat::Raw,
                    },
                )
                .unwrap_err(),
            PushError::CannotBackOnto {
                role: role("disk"),
                digest: dg(2),
            },
        );
    }

    #[test]
    fn building_validates_the_image() {
        assert_eq!(ImageBuilder::default().build(), Err(ImageError::NoRole));

        let mut builder = ImageBuilder::default();
        builder
            .push(role("disk"), qcow2(1, 8))
            .unwrap()
            .push(role("disk"), qcow2(1, 8))
            .unwrap();
        assert_eq!(builder.build(), Err(ImageError::DuplicateLayer(dg(1))));
    }
}
