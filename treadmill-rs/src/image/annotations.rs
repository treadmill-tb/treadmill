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

/// How a member of an image group is run (value of the [`TARGET`] annotation).
pub const TARGET: &str = "ci.treadmill.target";

/// Board model a member of an image group targets (free-form; descriptor-level).
pub const BOARD: &str = "ci.treadmill.board";

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

/// Value of the [`TARGET`] annotation: how an image-group member is run.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Target {
    /// A QEMU virtual machine.
    Qemu,
    /// An NBD netboot target running on a physical board.
    NbdNetboot,
}

impl Target {
    pub fn as_str(&self) -> &'static str {
        match self {
            Target::Qemu => "qemu",
            Target::NbdNetboot => "nbd-netboot",
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Target {
    type Err = UnknownValue;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "qemu" => Ok(Target::Qemu),
            "nbd-netboot" => Ok(Target::NbdNetboot),
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
    fn target_roundtrip() {
        for target in [Target::Qemu, Target::NbdNetboot] {
            assert_eq!(target.as_str().parse::<Target>().unwrap(), target);
        }
        assert_eq!(
            "container".parse::<Target>(),
            Err(UnknownValue("container".to_string())),
        );
        // The hyphenated value is the one we emit into manifests.
        assert_eq!(Target::NbdNetboot.to_string(), "nbd-netboot");
    }
}
