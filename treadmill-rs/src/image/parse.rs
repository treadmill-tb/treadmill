//! Reading and validating Treadmill structure off OCI manifests.
//!
//! OCI gives us a generic `ImageManifest` type; this module projects it onto the
//! Treadmill-meaningful view while validating the invariants the rest of the
//! system relies on. Image *sets* are not OCI artifacts: they are mutable,
//! generationed switchboard entities.
//!
//! # The image model
//!
//! Every layer declares its format through its `mediaType`, and may carry a
//! [role](annotations::ROLE). The format defines how layers relate: a qcow2
//! layer may back onto one lower layer, named by digest. Layer order in the
//! manifest carries no meaning.
//!
//! The layers of known formats form **chains**. A chain's **head** is the one
//! layer no other layer backs onto, and it is exactly the layer carrying the
//! chain's role; the chain continues through the head's lowers down to its
//! base. A consumer asks for a chain by role, and the format assigns roles no
//! meaning of their own.
//!
//! [`TreadmillImage::new`] enforces, for every image:
//!
//! - each digest appears at most once, and each role at most once;
//! - the image has at least one role;
//! - a lower is present in the manifest, of a format that can back a qcow2
//!   layer (qcow2 or raw), and backs exactly one layer;
//! - a layer that is backed onto carries no role;
//! - a qcow2 layer's virtual size is not smaller than its lower's;
//! - every layer of a known format belongs to a chain (so there are no cycles
//!   and no stray layers).
//!
//! Layers of an unknown format are carried, but are opaque: they may carry a
//! role, and are exempt from the reachability rule, since links a newer format
//! defines are invisible here. A consumer that needs one refuses the image.
//!
//! A `dev.treadmill.<format>.*` annotation belongs to that format, and is
//! refused on a layer of any other format.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use oci_spec::image::{Descriptor, ImageManifest};

use super::annotations::{self, InvalidRole, Role};
use super::digest::{Digest, DigestParseError};
use super::media_types;

/// The annotation namespaces of the known formats, with the media type each
/// belongs to.
const FORMAT_NAMESPACES: [(&str, &str); 2] = [
    (annotations::QCOW2_NAMESPACE, media_types::QCOW2),
    (annotations::RAW_NAMESPACE, media_types::RAW),
];

/// The format of a layer's blob, with the properties that format defines.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LayerFormat {
    /// A qcow2 image ([`media_types::QCOW2`]).
    Qcow2 {
        /// The qcow2 virtual size, in bytes.
        virtual_size: u64,
        /// The layer this one backs onto, if any.
        lower: Option<Digest>,
    },
    /// Raw, uncompressed content ([`media_types::RAW`]).
    Raw,
    /// A format this version does not know.
    Unknown { media_type: String },
}

impl LayerFormat {
    /// The layer descriptor's `mediaType`.
    pub fn media_type(&self) -> &str {
        match self {
            LayerFormat::Qcow2 { .. } => media_types::QCOW2,
            LayerFormat::Raw => media_types::RAW,
            LayerFormat::Unknown { media_type } => media_type,
        }
    }

    /// Whether a qcow2 layer can back onto a layer of this format.
    pub fn can_back_qcow2(&self) -> bool {
        matches!(self, LayerFormat::Qcow2 { .. } | LayerFormat::Raw)
    }

    fn is_known(&self) -> bool {
        !matches!(self, LayerFormat::Unknown { .. })
    }
}

/// One layer (blob) of a Treadmill image, read off an OCI layer descriptor.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImageLayer {
    pub digest: Digest,
    /// Size of the blob, in bytes.
    pub size: u64,
    pub format: LayerFormat,
    /// Present exactly on the head of a chain.
    pub role: Option<Role>,
}

impl ImageLayer {
    /// The layer this one backs onto.
    pub fn lower(&self) -> Option<&Digest> {
        match &self.format {
            LayerFormat::Qcow2 { lower, .. } => lower.as_ref(),
            LayerFormat::Raw | LayerFormat::Unknown { .. } => None,
        }
    }

    /// Size of the content this layer presents to a consumer, in bytes: the
    /// qcow2 virtual size, or a raw blob's own size. Unknown for an unknown
    /// format.
    pub fn virtual_size(&self) -> Option<u64> {
        match &self.format {
            LayerFormat::Qcow2 { virtual_size, .. } => Some(*virtual_size),
            LayerFormat::Raw => Some(self.size),
            LayerFormat::Unknown { .. } => None,
        }
    }
}

/// Image-level metadata carried on the manifest annotations.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ImageMeta {
    pub title: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Name (or digest) of the image this one was derived from.
    pub base_name: Option<String>,
}

/// A validated Treadmill image.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TreadmillImage {
    /// In canonical order: chains sorted by role, each base first, followed by
    /// the role-less layers of unknown formats sorted by digest.
    layers: Vec<ImageLayer>,
    pub meta: ImageMeta,
}

/// The layers a role resolves to, base first.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Chain<'a> {
    /// Never empty; the last element is the head.
    layers: Vec<&'a ImageLayer>,
}

impl<'a> Chain<'a> {
    pub fn role(&self) -> &'a Role {
        self.head()
            .role
            .as_ref()
            .expect("a chain's head carries its role")
    }

    /// The chain's layers, base first and head last.
    pub fn layers(&self) -> &[&'a ImageLayer] {
        &self.layers
    }

    /// The layer carrying the role, which no other layer backs onto.
    pub fn head(&self) -> &'a ImageLayer {
        self.layers.last().expect("a chain is never empty")
    }

    /// Size of the content the chain presents, which is its head's.
    pub fn virtual_size(&self) -> Option<u64> {
        self.head().virtual_size()
    }
}

/// Why a manifest or a set of layers is not a valid Treadmill image.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ImageError {
    /// `artifactType` was missing, or not this version's Treadmill image type.
    UnsupportedArtifactType(Option<String>),
    /// A descriptor digest did not parse as a [`Digest`].
    BadDigest(DigestParseError),
    /// The role annotation was not a well-formed role name.
    InvalidRole { layer: Digest, error: InvalidRole },
    /// A qcow2 layer has no virtual size annotation.
    MissingVirtualSize(Digest),
    /// The virtual size annotation was not a decimal integer.
    BadVirtualSize { layer: Digest, value: String },
    /// The lower annotation was not a digest.
    BadLower {
        layer: Digest,
        value: String,
        error: DigestParseError,
    },
    /// An annotation of one format's namespace is on a layer of another.
    MisplacedAnnotation {
        layer: Digest,
        key: String,
        media_type: String,
    },
    /// A digest appears more than once among the layers.
    DuplicateLayer(Digest),
    /// A role is carried by more than one layer.
    DuplicateRole(Role),
    /// No layer carries a role.
    NoRole,
    /// A layer backs onto a digest the manifest does not carry.
    MissingLower { layer: Digest, lower: Digest },
    /// A layer backs onto a layer of a format that cannot back it.
    UnbackableLower {
        layer: Digest,
        lower: Digest,
        media_type: String,
    },
    /// Two layers back onto the same lower.
    SharedLower { lower: Digest, uppers: [Digest; 2] },
    /// A layer that is backed onto carries a role.
    RoleOnLower {
        layer: Digest,
        role: Role,
        upper: Digest,
    },
    /// A qcow2 layer is smaller than its lower, which would truncate it.
    ShrinkingVirtualSize {
        layer: Digest,
        virtual_size: u64,
        lower: Digest,
        lower_virtual_size: u64,
    },
    /// The lowers loop back to this layer.
    Cycle(Digest),
    /// A layer of a known format belongs to no role's chain.
    Unreachable(Digest),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::UnsupportedArtifactType(Some(found))
                if found.starts_with(media_types::IMAGE_ARTIFACT_TYPE_PREFIX) =>
            {
                write!(
                    f,
                    "unsupported Treadmill image version {found:?}, expected {:?}",
                    media_types::IMAGE_ARTIFACT_TYPE,
                )
            }
            ImageError::UnsupportedArtifactType(found) => write!(
                f,
                "manifest is not a Treadmill image: artifactType is {found:?}, expected {:?}",
                media_types::IMAGE_ARTIFACT_TYPE,
            ),
            ImageError::BadDigest(e) => write!(f, "invalid descriptor digest: {e}"),
            ImageError::InvalidRole { layer, error } => write!(f, "layer {layer}: {error}"),
            ImageError::MissingVirtualSize(layer) => write!(
                f,
                "qcow2 layer {layer} has no {} annotation",
                annotations::QCOW2_VIRTUAL_SIZE,
            ),
            ImageError::BadVirtualSize { layer, value } => write!(
                f,
                "layer {layer}: {} is not a decimal integer: {value:?}",
                annotations::QCOW2_VIRTUAL_SIZE,
            ),
            ImageError::BadLower {
                layer,
                value,
                error,
            } => write!(
                f,
                "layer {layer}: {} is not a digest ({error}): {value:?}",
                annotations::QCOW2_LOWER,
            ),
            ImageError::MisplacedAnnotation {
                layer,
                key,
                media_type,
            } => write!(
                f,
                "layer {layer} of media type {media_type} carries {key}, which belongs to \
                 another format",
            ),
            ImageError::DuplicateLayer(layer) => {
                write!(f, "layer {layer} appears more than once")
            }
            ImageError::DuplicateRole(role) => {
                write!(f, "role {role} is carried by more than one layer")
            }
            ImageError::NoRole => write!(f, "no layer carries a role"),
            ImageError::MissingLower { layer, lower } => write!(
                f,
                "layer {layer} backs onto {lower}, which is not one of the image's layers",
            ),
            ImageError::UnbackableLower {
                layer,
                lower,
                media_type,
            } => write!(
                f,
                "layer {layer} backs onto {lower}, whose media type {media_type} cannot back \
                 a qcow2 layer",
            ),
            ImageError::SharedLower { lower, uppers } => write!(
                f,
                "layers {} and {} both back onto {lower}",
                uppers[0], uppers[1],
            ),
            ImageError::RoleOnLower { layer, role, upper } => write!(
                f,
                "layer {layer} carries role {role}, but layer {upper} backs onto it; only a \
                 chain's head carries a role",
            ),
            ImageError::ShrinkingVirtualSize {
                layer,
                virtual_size,
                lower,
                lower_virtual_size,
            } => write!(
                f,
                "layer {layer} (virtual size {virtual_size}) is smaller than its lower {lower} \
                 (virtual size {lower_virtual_size}), which would truncate the chain",
            ),
            ImageError::Cycle(layer) => write!(f, "the lowers of layer {layer} form a cycle"),
            ImageError::Unreachable(layer) => {
                write!(f, "layer {layer} belongs to no role's chain")
            }
        }
    }
}

impl std::error::Error for ImageError {}

/// The roles an image provides are not the ones its consumer understands.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RoleMismatch {
    /// Roles the consumer needs that the image does not provide.
    pub missing: Vec<String>,
    /// Roles the image provides that the consumer does not understand.
    pub unexpected: Vec<Role>,
}

impl fmt::Display for RoleMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |roles: Vec<&str>| roles.join(", ");
        match (self.missing.is_empty(), self.unexpected.is_empty()) {
            (false, true) => write!(
                f,
                "image lacks role(s) {}",
                list(self.missing.iter().map(String::as_str).collect()),
            ),
            (true, false) => write!(
                f,
                "image provides role(s) {} this consumer does not understand",
                list(self.unexpected.iter().map(Role::as_str).collect()),
            ),
            _ => write!(
                f,
                "image lacks role(s) {}, and provides role(s) {} this consumer does not \
                 understand",
                list(self.missing.iter().map(String::as_str).collect()),
                list(self.unexpected.iter().map(Role::as_str).collect()),
            ),
        }
    }
}

impl std::error::Error for RoleMismatch {}

impl From<DigestParseError> for ImageError {
    fn from(e: DigestParseError) -> Self {
        ImageError::BadDigest(e)
    }
}

impl TreadmillImage {
    /// Validate `layers` as an image (see the [module docs](self)). The order
    /// of `layers` does not matter.
    pub fn new(mut layers: Vec<ImageLayer>, meta: ImageMeta) -> Result<Self, ImageError> {
        let order = validate(&layers)?;
        let position: HashMap<Digest, usize> =
            order.into_iter().enumerate().map(|(i, d)| (d, i)).collect();
        layers.sort_by_key(|layer| position[&layer.digest]);
        Ok(TreadmillImage { layers, meta })
    }

    /// All layers, in canonical order: chains sorted by role, each base first,
    /// followed by the role-less layers of unknown formats sorted by digest.
    pub fn layers(&self) -> &[ImageLayer] {
        &self.layers
    }

    pub fn layer(&self, digest: &Digest) -> Option<&ImageLayer> {
        self.layers.iter().find(|layer| layer.digest == *digest)
    }

    /// The roles the image provides, sorted.
    pub fn roles(&self) -> impl Iterator<Item = &Role> {
        self.layers.iter().filter_map(|layer| layer.role.as_ref())
    }

    /// The chain carrying `role`.
    pub fn chain(&self, role: &str) -> Option<Chain<'_>> {
        self.layers
            .iter()
            .find(|layer| layer.role.as_ref().is_some_and(|r| r == role))
            .map(|head| self.chain_from(head))
    }

    /// Every chain, sorted by role.
    pub fn chains(&self) -> impl Iterator<Item = Chain<'_>> {
        self.layers
            .iter()
            .filter(|layer| layer.role.is_some())
            .map(|head| self.chain_from(head))
    }

    /// Check that the image provides exactly the `expected` roles: a consumer
    /// cannot use an image lacking a role it needs, and must not silently
    /// ignore one it does not understand.
    pub fn check_roles(&self, expected: &[&str]) -> Result<(), RoleMismatch> {
        let mut missing: Vec<String> = expected
            .iter()
            .filter(|role| self.chain(role).is_none())
            .map(|role| role.to_string())
            .collect();
        missing.sort();
        missing.dedup();
        let unexpected: Vec<Role> = self
            .roles()
            .filter(|role| !expected.contains(&role.as_str()))
            .cloned()
            .collect();
        if missing.is_empty() && unexpected.is_empty() {
            Ok(())
        } else {
            Err(RoleMismatch {
                missing,
                unexpected,
            })
        }
    }

    pub fn into_parts(self) -> (Vec<ImageLayer>, ImageMeta) {
        (self.layers, self.meta)
    }

    fn chain_from<'a>(&'a self, head: &'a ImageLayer) -> Chain<'a> {
        let mut layers = vec![head];
        let mut cursor = head.lower();
        while let Some(digest) = cursor {
            let lower = self
                .layer(digest)
                .expect("a validated image carries every lower");
            layers.push(lower);
            cursor = lower.lower();
        }
        layers.reverse();
        Chain { layers }
    }
}

/// Check the invariants of the [module docs](self), and return the canonical
/// order of the layers' digests.
fn validate(layers: &[ImageLayer]) -> Result<Vec<Digest>, ImageError> {
    let mut by_digest: HashMap<Digest, &ImageLayer> = HashMap::with_capacity(layers.len());
    for layer in layers {
        if by_digest.insert(layer.digest, layer).is_some() {
            return Err(ImageError::DuplicateLayer(layer.digest));
        }
    }

    let mut roles = HashSet::new();
    for role in layers.iter().filter_map(|layer| layer.role.as_ref()) {
        if !roles.insert(role) {
            return Err(ImageError::DuplicateRole(role.clone()));
        }
    }
    if roles.is_empty() {
        return Err(ImageError::NoRole);
    }

    let mut upper_of: HashMap<Digest, Digest> = HashMap::new();
    for layer in layers {
        let Some(lower_digest) = layer.lower() else {
            continue;
        };
        let lower = by_digest
            .get(lower_digest)
            .ok_or(ImageError::MissingLower {
                layer: layer.digest,
                lower: *lower_digest,
            })?;
        if !lower.format.can_back_qcow2() {
            return Err(ImageError::UnbackableLower {
                layer: layer.digest,
                lower: lower.digest,
                media_type: lower.format.media_type().to_string(),
            });
        }
        if let Some(other) = upper_of.insert(lower.digest, layer.digest) {
            return Err(ImageError::SharedLower {
                lower: lower.digest,
                uppers: [other, layer.digest],
            });
        }
        if let Some(role) = &lower.role {
            return Err(ImageError::RoleOnLower {
                layer: lower.digest,
                role: role.clone(),
                upper: layer.digest,
            });
        }
        let virtual_size = layer.virtual_size().expect("a layer with a lower is qcow2");
        let lower_virtual_size = lower
            .virtual_size()
            .expect("a layer that can back qcow2 has a virtual size");
        if virtual_size < lower_virtual_size {
            return Err(ImageError::ShrinkingVirtualSize {
                layer: layer.digest,
                virtual_size,
                lower: lower.digest,
                lower_virtual_size,
            });
        }
    }

    // Walk every chain from its head. With each lower backing exactly one layer
    // and no head backed onto, the walks are disjoint and end.
    let mut heads: Vec<&ImageLayer> = layers.iter().filter(|l| l.role.is_some()).collect();
    heads.sort_by(|a, b| a.role.cmp(&b.role));
    let mut order = Vec::with_capacity(layers.len());
    let mut reached = HashSet::with_capacity(layers.len());
    for head in heads {
        let mut chain = Vec::new();
        let mut cursor = Some(head);
        while let Some(layer) = cursor {
            reached.insert(layer.digest);
            chain.push(layer.digest);
            cursor = layer.lower().map(|digest| by_digest[digest]);
        }
        chain.reverse();
        order.extend(chain);
    }

    let mut stray: Vec<&ImageLayer> = layers
        .iter()
        .filter(|layer| !reached.contains(&layer.digest))
        .collect();
    stray.sort_by_key(|layer| layer.digest);
    for layer in &stray {
        if layer.format.is_known() {
            // An unreached layer whose lowers lead back to it is a cycle; any
            // other is simply stray. A layer backed onto twice was refused
            // above, so no walk can enter a cycle without being part of it.
            let mut cursor = layer.lower();
            for _ in 0..layers.len() {
                let Some(digest) = cursor else { break };
                if *digest == layer.digest {
                    return Err(ImageError::Cycle(layer.digest));
                }
                cursor = by_digest[digest].lower();
            }
            return Err(ImageError::Unreachable(layer.digest));
        }
    }
    order.extend(stray.into_iter().map(|layer| layer.digest));

    Ok(order)
}

/// Parse and validate an OCI image manifest as a Treadmill image.
pub fn parse_image(manifest: &ImageManifest) -> Result<TreadmillImage, ImageError> {
    match manifest.artifact_type() {
        Some(mt) if mt.to_string() == media_types::IMAGE_ARTIFACT_TYPE => {}
        other => {
            return Err(ImageError::UnsupportedArtifactType(
                other.as_ref().map(|mt| mt.to_string()),
            ));
        }
    }

    let layers = manifest
        .layers()
        .iter()
        .map(decode_layer)
        .collect::<Result<Vec<_>, _>>()?;

    let manifest_annotations = manifest.annotations().as_ref();
    let annotation =
        |key: &str| -> Option<String> { manifest_annotations.and_then(|a| a.get(key)).cloned() };
    let meta = ImageMeta {
        title: annotation(annotations::oci::TITLE),
        version: annotation(annotations::oci::VERSION),
        description: annotation(annotations::oci::DESCRIPTION),
        base_name: annotation(annotations::oci::BASE_NAME),
    };

    TreadmillImage::new(layers, meta)
}

fn decode_layer(desc: &Descriptor) -> Result<ImageLayer, ImageError> {
    let digest = Digest::from_str(desc.digest().as_ref())?;
    let media_type = desc.media_type().to_string();
    let annotations = desc.annotations().clone().unwrap_or_default();

    let mut keys: Vec<&String> = annotations.keys().collect();
    keys.sort();
    for key in keys {
        for (namespace, owner) in FORMAT_NAMESPACES {
            if key.starts_with(namespace) && media_type != owner {
                return Err(ImageError::MisplacedAnnotation {
                    layer: digest,
                    key: key.clone(),
                    media_type,
                });
            }
        }
    }

    let role = annotations
        .get(annotations::ROLE)
        .map(|value| value.parse::<Role>())
        .transpose()
        .map_err(|error| ImageError::InvalidRole {
            layer: digest,
            error,
        })?;

    let format = match media_type.as_str() {
        media_types::QCOW2 => {
            let value = annotations
                .get(annotations::QCOW2_VIRTUAL_SIZE)
                .ok_or(ImageError::MissingVirtualSize(digest))?;
            let virtual_size = value
                .bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| value.parse::<u64>().ok())
                .flatten()
                .ok_or_else(|| ImageError::BadVirtualSize {
                    layer: digest,
                    value: value.clone(),
                })?;
            let lower = annotations
                .get(annotations::QCOW2_LOWER)
                .map(|value| {
                    Digest::from_str(value).map_err(|error| ImageError::BadLower {
                        layer: digest,
                        value: value.clone(),
                        error,
                    })
                })
                .transpose()?;
            LayerFormat::Qcow2 {
                virtual_size,
                lower,
            }
        }
        media_types::RAW => LayerFormat::Raw,
        _ => LayerFormat::Unknown {
            media_type: media_type.clone(),
        },
    };

    Ok(ImageLayer {
        digest,
        size: desc.size(),
        format,
        role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// A distinct, well-formed digest per small integer.
    fn digest(n: u8) -> Digest {
        Digest::from_sha256([n; 32])
    }

    fn role(name: &str) -> Role {
        name.parse().unwrap()
    }

    fn qcow2(n: u8, virtual_size: u64, lower: Option<u8>, role_name: Option<&str>) -> ImageLayer {
        ImageLayer {
            digest: digest(n),
            size: 10,
            format: LayerFormat::Qcow2 {
                virtual_size,
                lower: lower.map(digest),
            },
            role: role_name.map(role),
        }
    }

    fn raw(n: u8, size: u64, role_name: Option<&str>) -> ImageLayer {
        ImageLayer {
            digest: digest(n),
            size,
            format: LayerFormat::Raw,
            role: role_name.map(role),
        }
    }

    fn unknown(n: u8, role_name: Option<&str>) -> ImageLayer {
        ImageLayer {
            digest: digest(n),
            size: 10,
            format: LayerFormat::Unknown {
                media_type: "application/vnd.example.future".to_string(),
            },
            role: role_name.map(role),
        }
    }

    fn image(layers: Vec<ImageLayer>) -> Result<TreadmillImage, ImageError> {
        TreadmillImage::new(layers, ImageMeta::default())
    }

    fn chain_digests(image: &TreadmillImage, role: &str) -> Vec<Digest> {
        image
            .chain(role)
            .unwrap()
            .layers()
            .iter()
            .map(|layer| layer.digest)
            .collect()
    }

    const NETBOOT_MANIFEST: &str = r#"{
      "annotations": {
        "org.opencontainers.image.base.name": "ghcr.io/treadmill-tb/raspberrypios-13@sha256:e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1",
        "org.opencontainers.image.title": "Raspberry Pi OS 13 (NBD) with GitHub Actions Runner",
        "org.opencontainers.image.version": "13"
      },
      "artifactType": "application/vnd.treadmill.image.v2+json",
      "config": {
        "data": "e30=",
        "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
        "mediaType": "application/vnd.oci.empty.v1+json",
        "size": 2
      },
      "layers": [
        { "annotations": { "dev.treadmill.qcow2.lower": "sha256:a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
                           "dev.treadmill.qcow2.virtual-size": "6589251584",
                           "dev.treadmill.role": "rootfs" },
          "digest": "sha256:a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2",
          "mediaType": "application/vnd.treadmill.qcow2", "size": 734003200 },
        { "annotations": { "dev.treadmill.qcow2.virtual-size": "536870912" },
          "digest": "sha256:b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0",
          "mediaType": "application/vnd.treadmill.qcow2", "size": 71368704 },
        { "annotations": { "dev.treadmill.qcow2.virtual-size": "2294284288" },
          "digest": "sha256:a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0",
          "mediaType": "application/vnd.treadmill.qcow2", "size": 2085355520 },
        { "annotations": { "dev.treadmill.qcow2.lower": "sha256:b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0",
                           "dev.treadmill.qcow2.virtual-size": "536870912",
                           "dev.treadmill.role": "bootfs" },
          "digest": "sha256:b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1",
          "mediaType": "application/vnd.treadmill.qcow2", "size": 41943040 },
        { "annotations": { "dev.treadmill.qcow2.lower": "sha256:a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0",
                           "dev.treadmill.qcow2.virtual-size": "6589251584" },
          "digest": "sha256:a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
          "mediaType": "application/vnd.treadmill.qcow2", "size": 1073741824 }
      ],
      "mediaType": "application/vnd.oci.image.manifest.v1+json",
      "schemaVersion": 2
    }"#;

    fn parse_json(json: &str) -> Result<TreadmillImage, ImageError> {
        let manifest: ImageManifest = serde_json::from_str(json).unwrap();
        parse_image(&manifest)
    }

    /// A manifest with one layer, whose media type and annotations are given.
    fn one_layer_manifest(media_type: &str, annotations: &str) -> String {
        format!(
            r#"{{
              "artifactType": "application/vnd.treadmill.image.v2+json",
              "config": {{ "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                           "mediaType": "application/vnd.oci.empty.v1+json", "size": 2 }},
              "layers": [
                {{ "annotations": {{ {annotations} }},
                   "digest": "sha256:0101010101010101010101010101010101010101010101010101010101010101",
                   "mediaType": "{media_type}", "size": 10 }}
              ],
              "mediaType": "application/vnd.oci.image.manifest.v1+json",
              "schemaVersion": 2
            }}"#
        )
    }

    fn hex_digest(pair: &str) -> Digest {
        format!("sha256:{}", pair.repeat(32)).parse().unwrap()
    }

    #[test]
    fn parses_a_netboot_image_with_two_chains() {
        let img = parse_json(NETBOOT_MANIFEST).unwrap();

        assert_eq!(
            img.meta.title.as_deref(),
            Some("Raspberry Pi OS 13 (NBD) with GitHub Actions Runner"),
        );
        assert_eq!(img.meta.version.as_deref(), Some("13"));
        assert_eq!(img.meta.description, None);
        assert!(
            img.meta
                .base_name
                .as_deref()
                .unwrap()
                .starts_with("ghcr.io/")
        );

        assert_eq!(
            img.roles().map(Role::as_str).collect::<Vec<_>>(),
            ["bootfs", "rootfs"],
        );
        assert_eq!(
            chain_digests(&img, "bootfs"),
            [hex_digest("b0"), hex_digest("b1")],
        );
        assert_eq!(
            chain_digests(&img, "rootfs"),
            [hex_digest("a0"), hex_digest("a1"), hex_digest("a2")],
        );
        assert_eq!(img.chain("bootfs").unwrap().virtual_size(), Some(536870912));
        assert_eq!(
            img.chain("rootfs").unwrap().virtual_size(),
            Some(6589251584)
        );
        assert_eq!(img.chain("disk"), None);

        // Canonical order, whatever the manifest's order.
        assert_eq!(
            img.layers().iter().map(|l| l.digest).collect::<Vec<_>>(),
            ["b0", "b1", "a0", "a1", "a2"].map(hex_digest),
        );
    }

    #[test]
    fn a_v1_image_is_refused_as_an_unsupported_version() {
        let json = NETBOOT_MANIFEST.replace("image.v2+json", "image.v1+json");
        let error = parse_json(&json).unwrap_err();
        assert_eq!(
            error,
            ImageError::UnsupportedArtifactType(Some(
                "application/vnd.treadmill.image.v1+json".to_string()
            )),
        );
        assert!(
            error
                .to_string()
                .contains("unsupported Treadmill image version")
        );
    }

    #[test]
    fn a_manifest_without_the_artifact_type_is_refused() {
        let json = NETBOOT_MANIFEST.replace(
            r#""artifactType": "application/vnd.treadmill.image.v2+json","#,
            "",
        );
        assert_eq!(
            parse_json(&json),
            Err(ImageError::UnsupportedArtifactType(None)),
        );
    }

    #[test]
    fn a_qcow2_layer_needs_a_decimal_virtual_size() {
        assert_eq!(
            parse_json(&one_layer_manifest(
                media_types::QCOW2,
                r#""dev.treadmill.role": "disk""#
            )),
            Err(ImageError::MissingVirtualSize(digest(1))),
        );
        for value in ["not-a-number", "+5", "", "-1"] {
            assert_eq!(
                parse_json(&one_layer_manifest(
                    media_types::QCOW2,
                    &format!(
                        r#""dev.treadmill.role": "disk", "dev.treadmill.qcow2.virtual-size": "{value}""#
                    ),
                )),
                Err(ImageError::BadVirtualSize {
                    layer: digest(1),
                    value: value.to_string(),
                }),
            );
        }
    }

    #[test]
    fn a_malformed_role_is_refused() {
        assert_eq!(
            parse_json(&one_layer_manifest(
                media_types::RAW,
                r#""dev.treadmill.role": "Root""#
            )),
            Err(ImageError::InvalidRole {
                layer: digest(1),
                error: InvalidRole("Root".to_string()),
            }),
        );
    }

    #[test]
    fn a_format_annotation_on_another_format_is_refused() {
        for media_type in [media_types::RAW, "application/vnd.example.future"] {
            assert_eq!(
                parse_json(&one_layer_manifest(
                    media_type,
                    r#""dev.treadmill.role": "disk", "dev.treadmill.qcow2.virtual-size": "10""#,
                )),
                Err(ImageError::MisplacedAnnotation {
                    layer: digest(1),
                    key: annotations::QCOW2_VIRTUAL_SIZE.to_string(),
                    media_type: media_type.to_string(),
                }),
            );
        }
    }

    #[test]
    fn a_raw_head_and_an_unknown_head_are_single_layer_chains() {
        let img = parse_json(&one_layer_manifest(
            media_types::RAW,
            r#""dev.treadmill.role": "kernel", "example.org/ignored": "x""#,
        ))
        .unwrap();
        let chain = img.chain("kernel").unwrap();
        assert_eq!(chain.layers().len(), 1);
        assert_eq!(chain.virtual_size(), Some(10));

        let img = image(vec![unknown(1, Some("firmware"))]).unwrap();
        let chain = img.chain("firmware").unwrap();
        assert_eq!(chain.layers().len(), 1);
        assert_eq!(chain.virtual_size(), None);
    }

    #[test]
    fn a_qcow2_layer_can_back_onto_raw() {
        let img = image(vec![
            qcow2(2, 2 * GIB, Some(1), Some("disk")),
            raw(1, GIB, None),
        ])
        .unwrap();
        assert_eq!(chain_digests(&img, "disk"), [digest(1), digest(2)]);
    }

    #[test]
    fn a_role_less_layer_of_an_unknown_format_is_carried() {
        let img = image(vec![unknown(9, None), qcow2(1, GIB, None, Some("disk"))]).unwrap();
        assert_eq!(
            img.layers().iter().map(|l| l.digest).collect::<Vec<_>>(),
            [digest(1), digest(9)],
        );
    }

    #[test]
    fn chains_do_not_depend_on_layer_order() {
        let layers = vec![
            qcow2(3, 4 * GIB, Some(2), Some("disk")),
            qcow2(1, GIB, None, None),
            qcow2(2, 2 * GIB, Some(1), None),
        ];
        let mut reversed = layers.clone();
        reversed.reverse();

        let img = image(layers).unwrap();
        assert_eq!(
            chain_digests(&img, "disk"),
            [digest(1), digest(2), digest(3)]
        );
        assert_eq!(image(reversed).unwrap(), img);
    }

    #[test]
    fn duplicate_digests_and_roles_are_refused() {
        assert_eq!(
            image(vec![
                qcow2(1, GIB, None, Some("disk")),
                qcow2(1, GIB, None, Some("disk")),
            ]),
            Err(ImageError::DuplicateLayer(digest(1))),
        );
        assert_eq!(
            image(vec![
                qcow2(1, GIB, None, Some("disk")),
                qcow2(2, GIB, None, Some("disk")),
            ]),
            Err(ImageError::DuplicateRole(role("disk"))),
        );
    }

    #[test]
    fn an_image_without_a_role_is_refused() {
        assert_eq!(image(vec![]), Err(ImageError::NoRole));
        assert_eq!(image(vec![unknown(1, None)]), Err(ImageError::NoRole));
    }

    #[test]
    fn a_dangling_lower_is_refused() {
        assert_eq!(
            image(vec![qcow2(3, GIB, Some(9), Some("disk"))]),
            Err(ImageError::MissingLower {
                layer: digest(3),
                lower: digest(9),
            }),
        );
    }

    #[test]
    fn a_lower_of_an_unknown_format_is_refused() {
        assert_eq!(
            image(vec![qcow2(2, GIB, Some(1), Some("disk")), unknown(1, None),]),
            Err(ImageError::UnbackableLower {
                layer: digest(2),
                lower: digest(1),
                media_type: "application/vnd.example.future".to_string(),
            }),
        );
    }

    #[test]
    fn a_shared_lower_is_refused() {
        assert_eq!(
            image(vec![
                qcow2(1, GIB, None, None),
                qcow2(2, GIB, Some(1), Some("a")),
                qcow2(3, GIB, Some(1), Some("b")),
            ]),
            Err(ImageError::SharedLower {
                lower: digest(1),
                uppers: [digest(2), digest(3)],
            }),
        );
    }

    #[test]
    fn a_role_below_the_head_is_refused() {
        assert_eq!(
            image(vec![
                qcow2(1, GIB, None, Some("base")),
                qcow2(2, GIB, Some(1), Some("disk")),
            ]),
            Err(ImageError::RoleOnLower {
                layer: digest(1),
                role: role("base"),
                upper: digest(2),
            }),
        );
    }

    #[test]
    fn a_shrinking_virtual_size_is_refused() {
        assert_eq!(
            image(vec![
                raw(1, 2 * GIB, None),
                qcow2(2, GIB, Some(1), Some("disk")),
            ]),
            Err(ImageError::ShrinkingVirtualSize {
                layer: digest(2),
                virtual_size: GIB,
                lower: digest(1),
                lower_virtual_size: 2 * GIB,
            }),
        );
    }

    #[test]
    fn cycles_are_refused() {
        assert_eq!(
            image(vec![
                qcow2(1, GIB, None, Some("disk")),
                qcow2(2, GIB, Some(3), None),
                qcow2(3, GIB, Some(2), None),
            ]),
            Err(ImageError::Cycle(digest(2))),
        );
        assert_eq!(
            image(vec![
                qcow2(1, GIB, None, Some("disk")),
                qcow2(2, GIB, Some(2), None),
            ]),
            Err(ImageError::Cycle(digest(2))),
        );
    }

    #[test]
    fn roles_are_checked_against_the_expected_set() {
        let img = image(vec![
            qcow2(1, GIB, None, Some("bootfs")),
            qcow2(2, GIB, None, Some("rootfs")),
        ])
        .unwrap();

        assert_eq!(img.check_roles(&["rootfs", "bootfs"]), Ok(()));

        let error = img.check_roles(&["disk"]).unwrap_err();
        assert_eq!(
            error,
            RoleMismatch {
                missing: vec!["disk".to_string()],
                unexpected: vec![role("bootfs"), role("rootfs")],
            },
        );
        assert_eq!(
            error.to_string(),
            "image lacks role(s) disk, and provides role(s) bootfs, rootfs this consumer \
             does not understand",
        );

        assert_eq!(
            img.check_roles(&["bootfs", "rootfs", "kernel"])
                .unwrap_err()
                .to_string(),
            "image lacks role(s) kernel",
        );
    }

    #[test]
    fn a_stray_layer_is_refused() {
        assert_eq!(
            image(vec![qcow2(1, GIB, None, Some("disk")), raw(2, GIB, None),]),
            Err(ImageError::Unreachable(digest(2))),
        );
    }
}
