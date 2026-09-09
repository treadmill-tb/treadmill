use anyhow::{Context, bail, ensure};

use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::parse::{ImageLayer, TreadmillImage};

pub struct Split<'a> {
    pub roots: Vec<&'a ImageLayer>,
    pub boots: Vec<&'a ImageLayer>,
}

/// Split the layers by role and check that array order *is* backing-chain
/// order: each root layer backs onto its predecessor, the head names the last
/// one, and boot layers stand alone.
pub fn split_and_check_order(image: &TreadmillImage) -> anyhow::Result<Split<'_>> {
    let mut roots = Vec::new();
    let mut boots = Vec::new();
    for (i, layer) in image.layers.iter().enumerate() {
        match layer.role {
            Some(Role::Root) => roots.push(layer),
            Some(Role::Boot) => boots.push(layer),
            None => bail!(
                "layer {i} ({}) carries no dev.treadmill.role annotation",
                layer.digest,
            ),
        }
    }

    for (i, layer) in roots.iter().enumerate() {
        if i == 0 {
            ensure!(
                layer.lower.is_none(),
                "the first root layer ({}) must have no lower",
                layer.digest,
            );
        } else {
            ensure!(
                layer.lower.as_ref() == Some(&roots[i - 1].digest),
                "root layer {i} ({}) must back onto its predecessor in array order",
                layer.digest,
            );
        }
    }

    let head = roots.last().context("image has no role=root layer")?;
    ensure!(
        image.head == head.digest,
        "head {} must name the last root layer in array order ({})",
        image.head,
        head.digest,
    );

    for layer in &boots {
        ensure!(
            layer.lower.is_none(),
            "boot layer {} must have no lower",
            layer.digest,
        );
        ensure!(
            image.head != layer.digest,
            "boot layer {} must not be the head",
            layer.digest,
        );
    }

    Ok(Split { roots, boots })
}
