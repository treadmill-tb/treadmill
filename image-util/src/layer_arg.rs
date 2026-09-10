use std::path::PathBuf;
use std::str::FromStr;

use treadmill_rs::image::annotations::Role;

#[derive(Debug, Clone)]
pub struct LayerArg {
    pub role: Role,
    pub path: PathBuf,
}

impl FromStr for LayerArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (role, path) = s
            .split_once('=')
            .ok_or_else(|| format!("expected ROLE=PATH, got {s:?}"))?;
        let role = Role::from_str(role).map_err(|e| format!("invalid layer role {:?}", e.0))?;
        Ok(LayerArg {
            role,
            path: PathBuf::from(path),
        })
    }
}
