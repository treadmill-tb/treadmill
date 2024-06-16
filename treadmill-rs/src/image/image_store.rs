use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageStoreConfig {
    base_path: PathBuf,
}

// pub struct ImageStore {
//     _config: ImageStoreConfig,
// }

// impl ImageStore {
//     pub fn new(config: ImageStoreConfig) -> Self {
// 	ImageStore {
// 	    config,
// 	}
//     }
// }
