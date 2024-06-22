//! # The Treadmill Image Manifest Format.
//!
//! Treadmill images are described by an associated manifest. At its core, a
//! Treadmill image is a collection of files (called _blobs_).
//!
//! An image may hold one or more blobs, which can be useful for certain
//! targets: for instance, a virtual machine image may be composed of layers,
//! where each layer is represented by a blob in the image. Another example are
//! netboot targets, which typically consist of a "boot" file system exposed via
//! TFTP, and a "root" file system via NFS.
//!
//! Treadmill uses a content-addressed store to refer to both blobs, and images
//! in general. For instance, a Treadmill store may look like the following:
//!
//! ```text
//! treadmill-image-store/
//! ├── blobs
//! │   └── 5f/ec/eb
//! │       └── 5feceb66ffc86f38d952786c6d696c79c2dbc239dd4e91b46729d73a27fb57e9
//! │   └── 6b/86/b2
//! │       └── 6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b
//! └── images
//!     └── d4/73/5e
//!         └── d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35
//!             ├──> refers to blobs/5f/ec/eb/5feceb66ff...
//!             └──> refers to blobs/6b/86/b2/6b86b273ff...
//! ```
//!
//! All blobs are stored in a directory hierarchy generated from their SHA-256
//! digest, and are named by that digest. An image can refer to one or more
//! blobs. Images itself are represented through their manifests, and stored and
//! named according to the SHA-256 digest of their manifest.
//!
//! All entires in this store are immutable. Together, the above properties
//! ensure that the system has an efficient way of naming images, verifying
//! their integrity, and reducing storage requirements by deduplicating blobs
//! used by multiple images (such as a common base operating-system disk layer
//! for a virtual machine).
//!
//! Whereas two blobs with diverging checksums are always treated as disparate
//! entries in the store, two image manifests may have the same effective
//! contents, but are represented slightly differently. For instance, image
//! manifests are stored in the TOML format, which allows for arbitrary
//! whitespace and comments to be added to the file, and does not mandate an
//! order of keys within objects; such differences will cause the file's digest
//! to diverge.
//!
//! Thus, *Treadmill treats image manifests with identical contents but
//! different representations as disparate images*. When an image is retrieved
//! by its digest, the provided image must be a byte-wise identical copy of the
//! original supplied manifest file.
//!
//! ## Versioning
//!
//! Each image manifest contains a top-level `manifest_version` attribute, in
//! the following format: `[major].[minor]`, where `[major]` and `[minor]` are
//! positive integer values respectively. Minor version increments represent
//! backward-compatible changes (extensions), whereas manifest files are not
//! expected to preserve compatibility across major version increments.
//!
//! Currently, `manifest_version` is set to `0.0`.
//!
//! Unrecognized fields of a manifest must be ignored.
//!
//! ## Manifest Extensions
//!
//! Image manifests come in different forms: for example, whereas _base_
//! manifests are stored in image stores persistently and contain all attributes
//! necessary for them to be used, they do not contain information on where
//! individual blobs of an image can be fetched from. This information is
//! provided through an extension called _source_ manifests.
//!
//! The manifest extensions are encoded in a `manifest_extensions` attribute. It
//! must always contain `"org.tockos.treadmill.manifest-ext.base"`, and may
//! contain additional extensions.
//!
//! Currently there are two extensions defined:
//! - `org.tockos.treadmill.manifest-ext.base` and
//! - `org.tockos.treadmill.manifest-ext.source`.
//!
//! Manifest extensions can either be versioned independently (by adding a
//! version number into their tag), or change their semantics based on the
//! `manifest_version` attribute. For the latter, the must adhere to the
//! compatibility constraints stated under [Versioning](#versioning).
//!
//! Unknown manifest extensions should be ignored.
//!
//! Attributes introduced by an extension must be prefixed by its identifier,
//! following a single period character: `<org.foobar.baz>.<attribute-name>`.
//!
//! ## Base Manifest Format
//!
//! We define the following top-level attributes, prefixed by
//! `org.tockos.treadmill.manifest-ext.base.`:
//!
//! - `label`: `string`, short human-readable label describing the image;
//! - `revision`: `unsigned integer`, user-supplied revision number of this
//!   image;
//! - `description`: `string`, longer human-readable description describing the
//!   image and other important information, may contain Markdown-style markup;
//! - `attrs`: `map<string> -> string`, aribitrary attrbiutes attached to the
//!   image, attributes should be namespaced to avoid collisions;
//! - `blobs`: `map<string> -> `[`Blob Specification`](#blob-specification),
//!   individual labeled blobs comprising this image.
//!
//! ### Blob Specification
//!
//! For each blob specification object we define the following  attributes,
//! prefixed by `org.tockos.treadmill.manifest-ext.base.`:
//!
//! - `sha256_digest`: `string`, 64-character lower-case hex-encoded SHA256
//!   digest of the blob's contents;
//! - `size`: `unsigned integer`, size of the blob in byte;
//! - `attrs`: `map[string] -> string`, aribitrary attrbiutes attached to the
//!   image, attributes should be namespaced to avoid collisions.
//!
//! ## Source Manifest Extension
//!
//! TODO: specify...

use serde_with::serde_as;
use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Images are content-addressed by the SHA-256 checksum of their manifest.
///
/// This type can be formatted into a lower-case hex string, as expected in the
/// `org.tockos.treadmill.manifest-ext.base.sha256_digest` attribute.
#[serde_as]
#[derive(Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageId(#[serde_as(as = "serde_with::hex::Hex")] pub [u8; 32]);

impl fmt::Debug for ImageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ImageId")
            .field(&crate::util::hex_slice::HexSlice(&self.0))
            .finish()
    }
}

mod private {
    pub trait ManifestTypeSeal {}
    impl ManifestTypeSeal for super::BaseManifest {}
    impl ManifestTypeSeal for super::SourceManifest {}
}

/// Validate (parts of) an Image Manifest according to all known constraints
///
/// This trait serves as a method validate whether generated Image Manifests
/// will likely be accepted as _valid_ by other consumers, and to perform basic
/// validation on imported manifests.
pub trait Validate {
    fn validate(&self) -> bool;
}

/// Unconditionally-valid unit type (`()`).
///
/// We use the unit type as a placeholder for optional fields.
impl Validate for () {
    fn validate(&self) -> bool {
        true
    }
}

/// Type-parameter to distinguish between different "manifest configurations".
///
/// Depending on the assicated types, the manifest is expected to have certain
/// extension fields present.
pub trait ManifestType: private::ManifestTypeSeal {
    type BlobSpecSourceExt: std::fmt::Debug + Clone + Validate + Serialize + for<'a> Deserialize<'a>;
}

/// Base manifest "configuration".
///
/// No extension attributes are expected to be present, and none will be parsed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum BaseManifest {}

impl ManifestType for BaseManifest {
    type BlobSpecSourceExt = ();
}

/// Source manifest "configuration".
///
/// Only the `base` and `source` extension fields are expected to be present and
/// will be parsed and/or validated.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum SourceManifest {}

impl ManifestType for SourceManifest {
    type BlobSpecSourceExt = ImageBlobSpecSourceExt;
}

/// Base manifest type. This type is generic over the [`ManifestType`] trait,
/// and thus can be configured to expect, parse and validate any additional
/// extension attributes.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GenericImageManifest<T: ManifestType> {
    pub manifest_version: u64,
    pub manifest_extensions: Vec<String>,
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.label")]
    pub label: String,
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.revision")]
    pub revision: usize,
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.description")]
    pub description: String,
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.blobs")]
    pub blobs: HashMap<String, ImageBlobSpec<T>>,
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.attrs")]
    pub attrs: HashMap<String, String>,
}

/// Image manifest containing only the `org.tockos.treadmill.manifest-ext.base`
/// extension.
pub type ImageManifest = GenericImageManifest<BaseManifest>;

/// Image manifest featuring the `org.tockos.treadmill.manifest-ext.base` and
/// `org.tockos.treadmill.manifest-ext.source` extensions.
pub type ImageSourceManifest = GenericImageManifest<SourceManifest>;

impl<T: ManifestType> Validate for GenericImageManifest<T> {
    fn validate(&self) -> bool {
        self.label.len() <= 64
            && self.description.len() <= 64 * 1024
            && self.blobs.len() <= 4096
            && self
                .attrs
                .iter()
                .all(|(attr_name, attr_val)| attr_name.len() <= 64 && attr_val.len() <= 64 * 1024)
            && self.blobs.iter().all(|(blob_name, blob)| {
                blob_name.len() <= 64
                    && blob_name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    && blob.validate()
            })
    }
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImageBlobSpec<T: ManifestType> {
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.sha256-digest")]
    #[serde_as(as = "serde_with::hex::Hex")]
    pub sha256_digest: [u8; 32],
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.size")]
    pub size: u64,
    #[serde(rename = "org.tockos.treadmill.manifest-ext.base.attrs")]
    pub attrs: HashMap<String, String>,
    #[serde(flatten)]
    pub source_ext: T::BlobSpecSourceExt,
}

impl<T: ManifestType> Validate for ImageBlobSpec<T> {
    fn validate(&self) -> bool {
        self.attrs
            .iter()
            .all(|(attr_name, attr_val)| attr_name.len() <= 64 && attr_val.len() <= 64 * 1024)
            && self.source_ext.validate()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImageBlobSpecSourceExt {
    sources: Vec<ImageBlobSourceSpec>,
}

impl Validate for ImageBlobSpecSourceExt {
    fn validate(&self) -> bool {
        self.sources.iter().all(|s| s.validate())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ImageBlobSourceSpec {
    HttpGet {
        // TODO: maybe supporbmaint BasicAuth at some point?
        url: String,
    },
}

impl Validate for ImageBlobSourceSpec {
    fn validate(&self) -> bool {
        true
    }
}
