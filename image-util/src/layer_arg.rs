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

/// The format of a layer blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerFormatArg {
    Qcow2,
    Raw,
}

impl FromStr for LayerFormatArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "qcow2" => Ok(LayerFormatArg::Qcow2),
            "raw" => Ok(LayerFormatArg::Raw),
            _ => Err(format!("unknown layer format {s:?}, expected qcow2 or raw")),
        }
    }
}

/// A layer blob to place on top of a role's chain, as `ROLE=FORMAT:PATH`.
#[derive(Debug, Clone)]
pub struct LayerArg {
    pub role: Role,
    pub format: LayerFormatArg,
    pub path: PathBuf,
}

impl FromStr for LayerArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (role, value) = split_role(s, "FORMAT:PATH")?;
        let (format, path) = value
            .split_once(':')
            .ok_or_else(|| format!("expected ROLE=FORMAT:PATH, got {s:?}"))?;
        Ok(LayerArg {
            role,
            format: format.parse()?,
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
