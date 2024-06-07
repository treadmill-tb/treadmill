#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageStoreConfig {
    base_path: PathBuf,
}

pub struct ImageStore {
    config: ImageStoreConfig,
}

impl ImageStore {
    pub fn new(config: ImageStoreConfig) -> Self {
	ImageStore {
	    config,
	}
    }
}

