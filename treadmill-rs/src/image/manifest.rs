use std::collections::HashMap;
use std::fmt;
use serde_with::serde_as;

use serde::{Deserialize, Serialize};

/// Images are content-addressed by the SHA-256 checksum of their manifest.
#[serde_as]
#[derive(Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageId(
    #[serde_as(as = "serde_with::hex::Hex")]
    pub [u8; 32]
);

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

    pub attrs: HashMap<String, String>,
}

impl ImageManifest {
    pub fn validate(&self) -> bool {
        self.label.len() <= 64
            && self.description.len() <= 64 * 1024
            && self.parts.len() <= 4096
            && self
                .attrs
                .iter()
                .all(|(attr_name, attr_val)| attr_name.len() <= 64 && attr_val.len() <= 64 * 1024)
            && self.parts.iter().all(|(part_name, part)| {
                part_name.len() <= 64
                    && part_name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    && part.validate()
            })
    }
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImagePartSpec {
    // pub sources: Vec<ImagePartSourceSpec>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub sha256_digest: [u8; 32],
    pub size: u64,
    pub attrs: HashMap<String, String>,
}

impl ImagePartSpec {
    pub fn validate(&self) -> bool {
        self
            .attrs
            .iter()
            .all(|(attr_name, attr_val)| attr_name.len() <= 64 && attr_val.len() <= 64 * 1024)
            && self.sources.iter().all(|s| s.validate())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ImagePartSourceSpec {
    HttpGet {
        // TODO: maybe supporbmaint BasicAuth at some point?
        url: String,
    },
}

impl ImagePartSourceSpec {
    pub fn validate(&self) -> bool {
        true
    }
}
