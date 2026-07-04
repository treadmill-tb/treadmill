//! References to a concrete image at a concrete location.
//!
//! An image's *identity* is its manifest [`Digest`]; an [`ImageRef`] pairs that
//! identity with one *location* — a `(registry, repository)` that serves the
//! bytes. The same digest may be reachable through many `ImageRef`s (registry
//! redundancy, promotion to a canonical location); any of them is interchangeable
//! because a pull is always verified against the digest. See
//! `doc/oci-image-migration-plan.md` §5/§D16.
//!
//! The canonical string form is `registry/repository@sha256:<hex>`, where
//! `repository` may itself contain `/`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::digest::{Digest, DigestParseError};

/// A pinned image at a specific registry location.
#[derive(schemars::JsonSchema, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    /// Registry host, optionally with a port (e.g. `registry.example.com:5000`).
    pub registry: String,
    /// Repository path within the registry (e.g. `u/alice/ubuntu`); may contain
    /// `/`.
    pub repository: String,
    /// The pinned manifest digest — the image's content-addressed identity.
    pub digest: Digest,
}

/// Error parsing an [`ImageRef`] from its string form.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ImageRefParseError {
    /// No `@<digest>` suffix was present.
    MissingDigest,
    /// The `@<digest>` suffix did not parse as a [`Digest`].
    BadDigest(DigestParseError),
    /// No `/` separating the registry host from the repository path.
    MissingRepository,
    /// The registry host was empty.
    EmptyRegistry,
    /// The repository path was empty.
    EmptyRepository,
}

impl fmt::Display for ImageRefParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageRefParseError::MissingDigest => {
                write!(f, "image reference must end in `@<digest>`")
            }
            ImageRefParseError::BadDigest(e) => write!(f, "invalid digest: {e}"),
            ImageRefParseError::MissingRepository => {
                write!(f, "image reference must contain `<registry>/<repository>`")
            }
            ImageRefParseError::EmptyRegistry => write!(f, "registry host must not be empty"),
            ImageRefParseError::EmptyRepository => write!(f, "repository path must not be empty"),
        }
    }
}

impl std::error::Error for ImageRefParseError {}

impl FromStr for ImageRef {
    type Err = ImageRefParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split off the digest first: the digest never contains `@`, and the
        // name portion never does, so the last `@` is unambiguous.
        let (name, digest_str) = s
            .rsplit_once('@')
            .ok_or(ImageRefParseError::MissingDigest)?;
        let digest = digest_str
            .parse::<Digest>()
            .map_err(ImageRefParseError::BadDigest)?;

        // The registry host is everything up to the first `/`; the repository
        // (which may itself contain `/`) is the remainder.
        let (registry, repository) = name
            .split_once('/')
            .ok_or(ImageRefParseError::MissingRepository)?;

        if registry.is_empty() {
            return Err(ImageRefParseError::EmptyRegistry);
        }
        if repository.is_empty() {
            return Err(ImageRefParseError::EmptyRepository);
        }

        Ok(ImageRef {
            registry: registry.to_string(),
            repository: repository.to_string(),
            digest,
        })
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}@{}", self.registry, self.repository, self.digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIGEST: &str =
        "sha256:e839ce3984083b7c9b491615aa0382d159c5ee0204d252cce5efcf0225f1a622";

    #[test]
    fn parse_roundtrip_with_nested_repository() {
        let s = format!("registry.example.com:5000/u/alice/ubuntu@{SAMPLE_DIGEST}");
        let r: ImageRef = s.parse().unwrap();
        assert_eq!(r.registry, "registry.example.com:5000");
        assert_eq!(r.repository, "u/alice/ubuntu");
        assert_eq!(r.digest, SAMPLE_DIGEST.parse().unwrap());
        assert_eq!(r.to_string(), s);
    }

    #[test]
    fn rejects_missing_digest() {
        assert_eq!(
            "registry.example.com/ubuntu".parse::<ImageRef>(),
            Err(ImageRefParseError::MissingDigest),
        );
    }

    #[test]
    fn rejects_missing_repository() {
        assert_eq!(
            format!("registry.example.com@{SAMPLE_DIGEST}").parse::<ImageRef>(),
            Err(ImageRefParseError::MissingRepository),
        );
    }

    #[test]
    fn rejects_bad_digest() {
        let r = "registry.example.com/ubuntu@sha256:nothex".parse::<ImageRef>();
        assert!(matches!(r, Err(ImageRefParseError::BadDigest(_))));
    }

    #[test]
    fn serde_json_roundtrip() {
        let r: ImageRef = format!("registry.example.com/u/alice/ubuntu@{SAMPLE_DIGEST}")
            .parse()
            .unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: ImageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
