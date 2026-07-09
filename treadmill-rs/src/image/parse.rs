//! Reading and validating Treadmill structure off OCI manifests.
//!
//! OCI gives us a generic `ImageManifest` type; this module projects it onto the
//! Treadmill-meaningful view — a backing chain of qcow2 layers for an image —
//! while validating the invariants the rest of the system relies on (see
//! `doc/oci-image-migration-plan.md` §5). Image *sets* are not OCI artifacts:
//! they are mutable, generationed switchboard entities.

use std::str::FromStr;

use oci_spec::image::{Descriptor, ImageManifest};

use super::annotations::{self, Role};
use super::digest::{Digest, DigestParseError};
use super::media_types;

/// One layer (blob) of a Treadmill image, read off an OCI layer descriptor.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImageLayer {
    pub digest: Digest,
    pub size: u64,
    pub media_type: String,
    pub role: Option<Role>,
    pub virtual_size: Option<u64>,
    /// Digest of the layer immediately below this one in the backing chain.
    pub lower: Option<Digest>,
}

/// A validated Treadmill image, read off an OCI image manifest.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TreadmillImage {
    pub layers: Vec<ImageLayer>,
    /// Digest of the top (head) layer the chain is assembled from.
    pub head: Digest,
    pub title: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// Why an OCI manifest failed to parse as a Treadmill image.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ParseError {
    /// `artifactType` was missing or not the Treadmill image type.
    NotTreadmillImage,
    /// A descriptor digest did not parse as a [`Digest`].
    BadDigest(DigestParseError),
    /// The `ci.treadmill.role` annotation had an unrecognized value.
    UnknownRole(String),
    /// The `ci.treadmill.qcow2.virtual-size` annotation was not an integer.
    BadVirtualSize(String),
    /// No `ci.treadmill.qcow2.head` annotation on the manifest.
    MissingHead,
    /// The head digest does not name any layer in the manifest.
    HeadNotALayer(Digest),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NotTreadmillImage => {
                write!(f, "manifest is not a Treadmill image (wrong artifactType)")
            }
            ParseError::BadDigest(e) => write!(f, "invalid descriptor digest: {e}"),
            ParseError::UnknownRole(v) => write!(f, "unrecognized {}: {v:?}", annotations::ROLE),
            ParseError::BadVirtualSize(v) => {
                write!(
                    f,
                    "{} is not an integer: {v:?}",
                    annotations::QCOW2_VIRTUAL_SIZE
                )
            }
            ParseError::MissingHead => {
                write!(f, "manifest has no {} annotation", annotations::QCOW2_HEAD)
            }
            ParseError::HeadNotALayer(d) => {
                write!(f, "head {d} is not one of the manifest's layers")
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<DigestParseError> for ParseError {
    fn from(e: DigestParseError) -> Self {
        ParseError::BadDigest(e)
    }
}

fn descriptor_digest(desc: &Descriptor) -> Result<Digest, ParseError> {
    Ok(Digest::from_str(desc.digest().as_ref())?)
}

fn annotation<'a>(desc: &'a Descriptor, key: &str) -> Option<&'a String> {
    desc.annotations().as_ref().and_then(|a| a.get(key))
}

/// Parse and validate an OCI image manifest as a Treadmill image.
pub fn parse_image(manifest: &ImageManifest) -> Result<TreadmillImage, ParseError> {
    match manifest.artifact_type() {
        Some(mt) if mt.to_string() == media_types::IMAGE_ARTIFACT_TYPE => {}
        _ => return Err(ParseError::NotTreadmillImage),
    }

    let mut layers = Vec::with_capacity(manifest.layers().len());
    for desc in manifest.layers() {
        let role = annotation(desc, annotations::ROLE)
            .map(|v| Role::from_str(v).map_err(|e| ParseError::UnknownRole(e.0)))
            .transpose()?;
        let virtual_size = annotation(desc, annotations::QCOW2_VIRTUAL_SIZE)
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| ParseError::BadVirtualSize(v.clone()))
            })
            .transpose()?;
        let lower = annotation(desc, annotations::QCOW2_LOWER)
            .map(|v| Digest::from_str(v).map_err(ParseError::BadDigest))
            .transpose()?;

        layers.push(ImageLayer {
            digest: descriptor_digest(desc)?,
            size: desc.size(),
            media_type: desc.media_type().to_string(),
            role,
            virtual_size,
            lower,
        });
    }

    let manifest_annotations = manifest.annotations().as_ref();
    let head = manifest_annotations
        .and_then(|a| a.get(annotations::QCOW2_HEAD))
        .ok_or(ParseError::MissingHead)?;
    let head = Digest::from_str(head)?;
    if !layers.iter().any(|l| l.digest == head) {
        return Err(ParseError::HeadNotALayer(head));
    }

    let oci_annotation =
        |key: &str| -> Option<String> { manifest_annotations.and_then(|a| a.get(key)).cloned() };

    Ok(TreadmillImage {
        layers,
        head,
        title: oci_annotation(annotations::oci::TITLE),
        version: oci_annotation(annotations::oci::VERSION),
        description: oci_annotation(annotations::oci::DESCRIPTION),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "sha256:e839ce3984083b7c9b491615aa0382d159c5ee0204d252cce5efcf0225f1a622";
    const OVERLAY: &str = "sha256:3286aac796e2fcf217eb5a2f9430022aa16a6f1247182b35170b71cb196c6fe8";
    const EMPTY: &str = "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

    fn image_manifest_json(
        head: &str,
        virtual_size_overlay: &str,
        with_artifact_type: bool,
    ) -> String {
        let artifact = if with_artifact_type {
            r#""artifactType": "application/vnd.treadmill.image.v1+json","#
        } else {
            ""
        };
        format!(
            r#"{{
              "schemaVersion": 2,
              "mediaType": "application/vnd.oci.image.manifest.v1+json",
              {artifact}
              "config": {{ "mediaType": "application/vnd.oci.empty.v1+json", "digest": "{EMPTY}", "size": 2 }},
              "layers": [
                {{ "mediaType": "application/vnd.treadmill.disk.qcow2", "digest": "{BASE}", "size": 2085355520,
                   "annotations": {{ "ci.treadmill.role": "root", "ci.treadmill.qcow2.virtual-size": "2294284288" }} }},
                {{ "mediaType": "application/vnd.treadmill.disk.qcow2", "digest": "{OVERLAY}", "size": 3145728,
                   "annotations": {{ "ci.treadmill.role": "root", "ci.treadmill.qcow2.virtual-size": "{virtual_size_overlay}",
                                     "ci.treadmill.qcow2.lower": "{BASE}" }} }}
              ],
              "annotations": {{ "org.opencontainers.image.title": "Ubuntu test",
                                "org.opencontainers.image.version": "26.04",
                                "ci.treadmill.qcow2.head": "{head}" }}
            }}"#
        )
    }

    fn parse_json(json: &str) -> Result<TreadmillImage, ParseError> {
        let m: ImageManifest = serde_json::from_str(json).unwrap();
        parse_image(&m)
    }

    #[test]
    fn parses_two_layer_image() {
        let img = parse_json(&image_manifest_json(OVERLAY, "4294967296", true)).unwrap();
        assert_eq!(img.title.as_deref(), Some("Ubuntu test"));
        assert_eq!(img.version.as_deref(), Some("26.04"));
        assert_eq!(img.description, None);
        assert_eq!(img.head, OVERLAY.parse().unwrap());
        assert_eq!(img.layers.len(), 2);

        let base = &img.layers[0];
        assert_eq!(base.digest, BASE.parse().unwrap());
        assert_eq!(base.role, Some(Role::Root));
        assert_eq!(base.virtual_size, Some(2294284288));
        assert_eq!(base.lower, None);

        let overlay = &img.layers[1];
        assert_eq!(overlay.lower, Some(BASE.parse().unwrap()));
        assert_eq!(overlay.virtual_size, Some(4294967296));
        assert_eq!(overlay.media_type, media_types::DISK_QCOW2);
    }

    #[test]
    fn rejects_non_treadmill_artifact_type() {
        assert_eq!(
            parse_json(&image_manifest_json(OVERLAY, "4294967296", false)),
            Err(ParseError::NotTreadmillImage),
        );
    }

    #[test]
    fn rejects_head_not_a_layer() {
        let r = parse_json(&image_manifest_json(EMPTY, "4294967296", true));
        assert_eq!(r, Err(ParseError::HeadNotALayer(EMPTY.parse().unwrap())));
    }

    #[test]
    fn rejects_bad_virtual_size() {
        assert_eq!(
            parse_json(&image_manifest_json(OVERLAY, "not-a-number", true)),
            Err(ParseError::BadVirtualSize("not-a-number".to_string())),
        );
    }
}
