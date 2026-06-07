//! Annotation keys (and typed values) for Treadmill OCI images.
//!
//! Selection axes and backing-chain structure that don't fit the standard OCI
//! `platform`/descriptor fields are carried as annotations under the
//! `ci.treadmill.*` namespace. See `doc/oci-image-migration-plan.md` §D2/§D3/§D4.

use std::fmt;
use std::str::FromStr;

/// Role of a blob within an image (value of the [`ROLE`] annotation).
pub const ROLE: &str = "ci.treadmill.role";

/// Digest of the top (head) layer of a qcow2 backing chain (manifest-level).
pub const QCOW2_HEAD: &str = "ci.treadmill.qcow2.head";

/// Digest of the layer immediately below this one in a qcow2 backing chain
/// (descriptor-level).
pub const QCOW2_LOWER: &str = "ci.treadmill.qcow2.lower";

/// Advertised qcow2 virtual size of a layer, in bytes (descriptor-level).
pub const QCOW2_VIRTUAL_SIZE: &str = "ci.treadmill.qcow2.virtual-size";

/// Eligibility criteria of an image-group member: the set of host tags a host
/// must carry (as a superset) for this member to be selectable on it
/// (descriptor-level). The value is a comma-separated list of opaque tag strings
/// (e.g. `arch=arm64,raspberrypi-4`); see [`parse_tag_list`].
pub const REQUIRED_HOST_TAGS: &str = "ci.treadmill.required-host-tags";

/// Parse a [`REQUIRED_HOST_TAGS`]-style annotation value into its tags: split on
/// commas, trim each, and drop empties. Tags are otherwise opaque (the matcher
/// never interprets their internal structure).
pub fn parse_tag_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Standard OCI annotation keys that Treadmill populates.
pub mod oci {
    /// Human-readable image/title label.
    pub const TITLE: &str = "org.opencontainers.image.title";
    /// Longer human-readable description.
    pub const DESCRIPTION: &str = "org.opencontainers.image.description";
    /// User-supplied version/revision.
    pub const VERSION: &str = "org.opencontainers.image.version";
    /// Name of the image this one was derived from.
    pub const BASE_NAME: &str = "org.opencontainers.image.base.name";
}

/// An annotation carried a value outside the set this enum understands.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnknownValue(pub String);

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unrecognized annotation value: {:?}", self.0)
    }
}

impl std::error::Error for UnknownValue {}

/// Value of the [`ROLE`] annotation: what a blob is within an image.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Role {
    /// A root filesystem / disk layer.
    Root,
    /// A netboot boot filesystem.
    Boot,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Root => "root",
            Role::Boot => "boot",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "root" => Ok(Role::Root),
            "boot" => Ok(Role::Boot),
            other => Err(UnknownValue(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_roundtrip() {
        for role in [Role::Root, Role::Boot] {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
        }
        assert_eq!(
            "tmpfs".parse::<Role>(),
            Err(UnknownValue("tmpfs".to_string())),
        );
    }

    #[test]
    fn tag_list_splits_trims_and_drops_empties() {
        assert_eq!(
            parse_tag_list("arch=arm64, raspberrypi-4 ,,ble"),
            vec![
                "arch=arm64".to_string(),
                "raspberrypi-4".to_string(),
                "ble".to_string(),
            ],
        );
        assert!(parse_tag_list("").is_empty());
        assert!(parse_tag_list("  ,  ").is_empty());
    }
}
