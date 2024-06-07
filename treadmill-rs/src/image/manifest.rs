#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImageManifest {
    pub id: Uuid,

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

	for (part_name, part) in self.parts.iter() {
	    if part_name.len() > 64 || !part_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || !part.validate() {
		return false;
	    }
	}
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImagePartSpec {
    pub sources: Vec<ImagePartSourceSpec>,
    pub sha256_checksum: String,
}

impl ImagePartSpec {
    pub fn validate(&self) -> bool {
	if sha256_checksum.len() != 64 || !sha256_checksum.chars().all(|c| c.is_ascii_alphanumeric()) {
	    return false;
	}


	return self.sources.all(|s| s.validate());
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ImagePartSourceSpec {
    HttpGet {
	// TODO: maybe support BasicAuth at some point?
	url: String,
    }
}

impl ImagePartSourceSpec {
    pub fn validate(&self) -> bool {
	true
    }
}
