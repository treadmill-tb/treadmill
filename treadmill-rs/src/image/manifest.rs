use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Images are content-addressed by the SHA-256 checksum of their manifest.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct ImageId([u8; 32]);

impl fmt::Debug for ImageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ImageId")
            .field(&crate::util::hex_slice::HexSlice(&self.0))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImageManifest {
    pub label: String,
    pub revision: usize,
    pub description: String,

    pub parts: HashMap<String, ImagePartSpec>,
}

impl ImageManifest {
    pub fn validate(&self) -> bool {
        if self.label.len() > 64 || self.description.len() > 64 * 1024 || self.parts.len() > 4096 {
            return false;
        }

        self.parts.iter().all(|(part_name, part)| {
            part_name.len() <= 64
                && part_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && part.validate()
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImagePartSpec {
    pub sources: Vec<ImagePartSourceSpec>,
    pub sha256_checksum: String,
}

impl ImagePartSpec {
    pub fn validate(&self) -> bool {
        if self.sha256_checksum.len() != 64
            || !self
                .sha256_checksum
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
        {
            return false;
        }

        self.sources.iter().all(|s| s.validate())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ImagePartSourceSpec {
    HttpGet {
        // TODO: maybe support BasicAuth at some point?
        url: String,
    },
}

impl ImagePartSourceSpec {
    pub fn validate(&self) -> bool {
        true
    }
}
