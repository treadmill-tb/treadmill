//! OCI content digests.
//!
//! Under the OCI migration (see `doc/oci-image-migration-plan.md`), every blob,
//! image manifest, and image index is addressed by its OCI digest. Treadmill
//! only ever emits and accepts **SHA-256** digests, the universally-supported
//! OCI algorithm — other algorithms are rejected at parse time rather than
//! silently mishandled.
//!
//! The canonical string form is `sha256:<64 lower-case hex chars>`. Use
//! [`Digest::hex`] for the bare hex (e.g. to build a `blobs/sha256/<hex>` store
//! path) and [`Digest::encoded`] for the algorithm-prefixed form.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::util::hex_slice::HexSlice;

/// The only digest algorithm Treadmill produces or accepts.
pub const ALGORITHM: &str = "sha256";

/// A content-addressable OCI digest over 32 bytes of SHA-256.
///
/// Formats as the canonical OCI string `sha256:<64 lower-case hex>`.
#[derive(schemars::JsonSchema, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Digest(#[schemars(with = "String")] [u8; 32]);

impl Digest {
    /// Construct a digest from the raw 32-byte SHA-256 hash.
    pub const fn from_sha256(bytes: [u8; 32]) -> Self {
        Digest(bytes)
    }

    /// The raw 32-byte SHA-256 hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The bare lower-case hex (no `sha256:` prefix), e.g. for store paths.
    pub fn hex(&self) -> String {
        HexSlice(&self.0).to_string()
    }

    /// The canonical OCI form, `sha256:<hex>`.
    pub fn encoded(&self) -> String {
        format!("{ALGORITHM}:{}", self.hex())
    }
}

/// Error parsing a [`Digest`] from its string form.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DigestParseError {
    /// The algorithm prefix was missing or not `sha256:`.
    UnsupportedAlgorithm,
    /// The hex portion was not exactly 64 lower-case hex characters.
    MalformedHex,
}

impl fmt::Display for DigestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DigestParseError::UnsupportedAlgorithm => {
                write!(f, "digest must use the `{ALGORITHM}:` algorithm prefix")
            }
            DigestParseError::MalformedHex => {
                write!(f, "digest must be 64 lower-case hex characters")
            }
        }
    }
}

impl std::error::Error for DigestParseError {}

impl FromStr for Digest {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix(ALGORITHM)
            .and_then(|rest| rest.strip_prefix(':'))
            .ok_or(DigestParseError::UnsupportedAlgorithm)?;

        decode_hex32(hex)
            .map(Digest)
            .ok_or(DigestParseError::MalformedHex)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", ALGORITHM, HexSlice(&self.0))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Digest").field(&self.encoded()).finish()
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Digest::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Decode exactly 64 lower-case hex characters into 32 bytes.
///
/// OCI digests are canonically lower-case, so upper-case input is rejected to
/// keep a single byte-exact representation.
fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }

    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[2 * i])?;
        let lo = hex_val(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real digest from one of the sample image manifests.
    const SAMPLE: &str = "e839ce3984083b7c9b491615aa0382d159c5ee0204d252cce5efcf0225f1a622";

    #[test]
    fn parse_roundtrip() {
        let encoded = format!("sha256:{SAMPLE}");
        let d: Digest = encoded.parse().unwrap();
        assert_eq!(d.hex(), SAMPLE);
        assert_eq!(d.encoded(), encoded);
        assert_eq!(d.to_string(), encoded);
        assert_eq!(d.encoded().parse::<Digest>().unwrap(), d);
    }

    #[test]
    fn from_sha256_matches_parse() {
        let d = Digest::from_sha256([0u8; 32]);
        assert_eq!(d.hex(), "0".repeat(64));
        assert_eq!(d.encoded(), format!("sha256:{}", "0".repeat(64)));
        let parsed: Digest = d.encoded().parse().unwrap();
        assert_eq!(parsed, d);
    }

    #[test]
    fn rejects_wrong_algorithm() {
        assert_eq!(
            format!("md5:{SAMPLE}").parse::<Digest>(),
            Err(DigestParseError::UnsupportedAlgorithm),
        );
        assert_eq!(
            SAMPLE.parse::<Digest>(),
            Err(DigestParseError::UnsupportedAlgorithm),
        );
    }

    #[test]
    fn rejects_malformed_hex() {
        // Wrong length.
        assert_eq!(
            "sha256:abcd".parse::<Digest>(),
            Err(DigestParseError::MalformedHex),
        );
        // Non-hex character.
        assert_eq!(
            format!("sha256:{}", "z".repeat(64)).parse::<Digest>(),
            Err(DigestParseError::MalformedHex),
        );
        // Upper-case is not canonical.
        assert_eq!(
            format!("sha256:{}", SAMPLE.to_uppercase()).parse::<Digest>(),
            Err(DigestParseError::MalformedHex),
        );
    }

    #[test]
    fn serde_json_roundtrip() {
        let d: Digest = format!("sha256:{SAMPLE}").parse().unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"sha256:{SAMPLE}\""));
        let back: Digest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }
}
