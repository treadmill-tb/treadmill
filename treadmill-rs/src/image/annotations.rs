//! Annotation keys (and typed values) for Treadmill OCI images.
//!
//! Everything Treadmill-specific about an image is carried on its layer
//! descriptors, under the `dev.treadmill.*` namespace:
//!
//! - [`ROLE`] is independent of the layer's format, and names what a consumer
//!   uses the layer for.
//!
//! - `dev.treadmill.<format>.*` keys belong to one format, and are only valid
//!   on layers of that format. How layers relate to each other (a qcow2 backing
//!   chain) is such a format-specific property.

use std::fmt;
use std::str::FromStr;

/// Role of a layer within an image (value is a [`Role`]). Only a head layer,
/// one no other layer builds upon, carries a role.
pub const ROLE: &str = "dev.treadmill.role";

/// Namespace of the annotations belonging to the qcow2 format.
pub const QCOW2_NAMESPACE: &str = "dev.treadmill.qcow2.";

/// Digest of the layer a qcow2 layer backs onto. That layer must be qcow2 or
/// raw.
pub const QCOW2_LOWER: &str = "dev.treadmill.qcow2.lower";

/// The qcow2 virtual size of a layer, in bytes (a decimal integer). Required on
/// every qcow2 layer.
pub const QCOW2_VIRTUAL_SIZE: &str = "dev.treadmill.qcow2.virtual-size";

/// Namespace of the annotations belonging to the raw format. It currently
/// defines none.
pub const RAW_NAMESPACE: &str = "dev.treadmill.raw.";

/// Standard OCI annotation keys that Treadmill populates.
pub mod oci {
    /// Human-readable image/title label.
    pub const TITLE: &str = "org.opencontainers.image.title";
    /// Longer human-readable description.
    pub const DESCRIPTION: &str = "org.opencontainers.image.description";
    /// User-supplied version/revision.
    pub const VERSION: &str = "org.opencontainers.image.version";
    /// Image creation time (RFC 3339).
    pub const CREATED: &str = "org.opencontainers.image.created";
    /// Source control revision of the image build.
    pub const REVISION: &str = "org.opencontainers.image.revision";
    /// URL of documentation for the image.
    pub const DOCUMENTATION: &str = "org.opencontainers.image.documentation";
    /// Name of the image this one was derived from.
    pub const BASE_NAME: &str = "org.opencontainers.image.base.name";
}

/// Longest [`Role`] name accepted.
pub const ROLE_MAX_LEN: usize = 63;

/// The name of a role, e.g. `disk` or `rootfs`.
///
/// The image format assigns roles no meaning: a consumer (e.g. a supervisor)
/// defines the roles it understands. A role name starts with a lower-case
/// ASCII letter, continues with lower-case ASCII letters, digits and `-`, and
/// is at most [`ROLE_MAX_LEN`] characters long.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Role(String);

impl Role {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for Role {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Role {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// A role name was not well-formed.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InvalidRole(pub String);

impl fmt::Display for InvalidRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid role {:?}: expected a lower-case ASCII letter followed by at most {} \
             lower-case ASCII letters, digits or '-'",
            self.0,
            ROLE_MAX_LEN - 1,
        )
    }
}

impl std::error::Error for InvalidRole {}

impl FromStr for Role {
    type Err = InvalidRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let well_formed = s.len() <= ROLE_MAX_LEN
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if well_formed {
            Ok(Role(s.to_string()))
        } else {
            Err(InvalidRole(s.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_roles_parse() {
        for name in ["disk", "rootfs", "boot-fs2", "a", &"a".repeat(ROLE_MAX_LEN)] {
            assert_eq!(name.parse::<Role>().unwrap().as_str(), name);
        }
    }

    #[test]
    fn malformed_roles_are_refused() {
        for name in [
            "",
            "Disk",
            "2disk",
            "-disk",
            "root_fs",
            "root.fs",
            "rööt",
            &"a".repeat(ROLE_MAX_LEN + 1),
        ] {
            assert_eq!(name.parse::<Role>(), Err(InvalidRole(name.to_string())));
        }
    }
}
