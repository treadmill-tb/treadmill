use std::path::PathBuf;
use std::str::FromStr;

use treadmill_rs::image::annotations::Role;

/// A `ROLE=VALUE` command-line argument.
fn split_role(s: &str, value_name: &str) -> Result<(Role, String), String> {
    let (role, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected ROLE={value_name}, got {s:?}"))?;
    let role = Role::from_str(role).map_err(|e| e.to_string())?;
    Ok((role, value.to_string()))
}

/// A layer blob to place on top of a role's chain, as `ROLE=PATH`.
#[derive(Debug, Clone)]
pub struct LayerArg {
    pub role: Role,
    pub path: PathBuf,
}

impl FromStr for LayerArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (role, path) = split_role(s, "PATH")?;
        Ok(LayerArg {
            role,
            path: PathBuf::from(path),
        })
    }
}

/// The expected length of a role's chain, as `ROLE=LAYERS`.
#[derive(Debug, Clone)]
pub struct ChainArg {
    pub role: Role,
    pub layers: usize,
}

impl FromStr for ChainArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (role, layers) = split_role(s, "LAYERS")?;
        let layers = layers
            .parse()
            .map_err(|_| format!("invalid layer count {layers:?}"))?;
        Ok(ChainArg { role, layers })
    }
}
