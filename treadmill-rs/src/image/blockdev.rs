//! Runtime assembly of a backing chain for `qemu-system-*` and
//! `qemu-storage-daemon`.
//!
//! Treadmill never bakes backing paths into the shared qcow2 blobs: the
//! chain is supplied at launch. This module turns an ordered chain — shared
//! read-only lower layers (base first) plus the per-job writable qcow2 overlay
//! on top — into a `-blockdev` node graph ([`BackingChain::blockdev_args`]):
//! each layer is a `file` node feeding a format node (`qcow2`, or `raw` for a
//! raw base), lowers pinned `read-only=on`, the overlay writable, exposed at
//! [`BackingChain::top_node`].
//!
//! Both supervisors share **one** form — the `-blockdev` node graph:
//!
//! - `qemu-system-*` takes the nodes directly and attaches its device to the
//!   chain's [`BackingChain::top_node`];
//! - the netboot path feeds the *same* nodes to `qemu-storage-daemon` and adds
//!   an NBD `--export` of each chain's [`BackingChain::top_node`].
//!
//! Each chain's node names start with its prefix, so several chains can share
//! one node graph.
//!
//! This supersedes the earlier `qemu-nbd --image-opts` form:
//! `qemu-nbd`/`qemu-img` `--image-opts` is a single `QemuOpts` blockdev and
//! **cannot express an inline backing node** (`qcow2` rejects `backing.driver`),
//! so an unbaked multi-layer chain is impossible to convey that way.
//! `qemu-storage-daemon` accepts the full node graph and exports it over NBD, so
//! both runtimes use the node-graph emitter and the per-target divergence is
//! just the NBD server/export flags (a supervisor concern).
//!
//! The base layer is opened with no backing reference, so a stray baked backing
//! is never followed (our blobs have none regardless).
//!
//! Paths are emitted verbatim into comma-separated key/value option strings;
//! qemu's keyval parser has no escape for `,` in a value, so this assumes blob
//! and overlay paths contain no commas (the daemon's content-addressed store
//! paths do not).

use std::fmt;
use std::path::{Path, PathBuf};

use super::digest::Digest;
use super::parse::{Chain, LayerFormat};

/// The qemu block driver a lower layer is opened with.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Driver {
    Qcow2,
    Raw,
}

impl Driver {
    fn name(self) -> &'static str {
        match self {
            Driver::Qcow2 => "qcow2",
            Driver::Raw => "raw",
        }
    }
}

/// A layer of a chain that no qemu block driver here can open.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnsupportedLayer {
    pub digest: Digest,
    pub media_type: String,
}

impl fmt::Display for UnsupportedLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "layer {} has media type {}, which cannot be opened as a block device",
            self.digest, self.media_type,
        )
    }
}

impl std::error::Error for UnsupportedLayer {}

/// A backing chain ready to be handed to a runtime.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackingChain {
    prefix: String,
    /// Shared read-only lower layers, **base first … head last**. Only the base
    /// may be raw, since a raw node has no backing.
    lowers: Vec<(PathBuf, Driver)>,
    /// The per-job writable qcow2 overlay layered on top of the head (created
    /// with no baked backing).
    overlay: PathBuf,
}

impl BackingChain {
    const DEFAULT_PREFIX: &'static str = "tml";

    /// Build a chain of qcow2 lowers (base first) and the per-job writable
    /// overlay, with the default node name prefix.
    pub fn new(lowers: Vec<PathBuf>, overlay: impl Into<PathBuf>) -> Self {
        Self::with_prefix(Self::DEFAULT_PREFIX, lowers, overlay)
    }

    /// Like [`BackingChain::new`], with the node names starting with `prefix`.
    pub fn with_prefix(prefix: &str, lowers: Vec<PathBuf>, overlay: impl Into<PathBuf>) -> Self {
        BackingChain {
            prefix: prefix.to_string(),
            lowers: lowers
                .into_iter()
                .map(|path| (path, Driver::Qcow2))
                .collect(),
            overlay: overlay.into(),
        }
    }

    /// Build the chain of an image's `chain`, with `blob_path` locating each
    /// layer's blob, and the node names starting with `prefix`.
    pub fn from_chain(
        prefix: &str,
        chain: &Chain<'_>,
        blob_path: impl Fn(&Digest) -> PathBuf,
        overlay: impl Into<PathBuf>,
    ) -> Result<Self, UnsupportedLayer> {
        let lowers = chain
            .layers()
            .iter()
            .map(|layer| {
                let driver = match layer.format {
                    LayerFormat::Qcow2 { .. } => Driver::Qcow2,
                    // A validated chain only has a raw layer at its base.
                    LayerFormat::Raw => Driver::Raw,
                    LayerFormat::Unknown { ref media_type } => {
                        return Err(UnsupportedLayer {
                            digest: layer.digest,
                            media_type: media_type.clone(),
                        });
                    }
                };
                Ok((blob_path(&layer.digest), driver))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BackingChain {
            prefix: prefix.to_string(),
            lowers,
            overlay: overlay.into(),
        })
    }

    /// `node-name` of the top (writable) qcow2 node the `-blockdev` graph
    /// exposes; a qemu device's `drive=` or an NBD export's `node-name=`
    /// references this.
    pub fn top_node(&self) -> String {
        format!("{}-disk", self.prefix)
    }

    fn lower_node(&self, i: usize) -> String {
        format!("{}-lower-{i}", self.prefix)
    }

    /// The `-blockdev` option strings (the values after each `-blockdev`), in
    /// dependency order so each node references one already defined. The caller
    /// passes each as its own `-blockdev <value>` and attaches the device to
    /// [`BackingChain::top_node`].
    pub fn blockdev_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity((self.lowers.len() + 1) * 2);

        for (i, (path, driver)) in self.lowers.iter().enumerate() {
            let fmt_node = self.lower_node(i);
            let file_node = format!("{fmt_node}-file");

            args.push(format!(
                "driver=file,node-name={file_node},filename={},read-only=on",
                path.display(),
            ));

            // The base (i == 0) is opened with no backing; every other lower
            // backs onto the one below it. All lowers are read-only and shared.
            let mut node = format!(
                "driver={},node-name={fmt_node},file={file_node},read-only=on",
                driver.name(),
            );
            if i > 0 {
                node.push_str(&format!(",backing={}", self.lower_node(i - 1)));
            }
            args.push(node);
        }

        // The writable per-job overlay on top.
        let top_node = self.top_node();
        let top_file = format!("{top_node}-file");
        args.push(format!(
            "driver=file,node-name={top_file},filename={}",
            self.overlay.display(),
        ));
        let mut top = format!("driver=qcow2,node-name={top_node},file={top_file}");
        if let Some(last) = self.lowers.len().checked_sub(1) {
            top.push_str(&format!(",backing={}", self.lower_node(last)));
        }
        args.push(top);

        args
    }

    /// The blob files the chain reads, base first (excludes the per-job overlay).
    pub fn lower_paths(&self) -> impl Iterator<Item = &Path> {
        self.lowers.iter().map(|(path, _)| path.as_path())
    }

    /// The per-job writable overlay path.
    pub fn overlay_path(&self) -> &Path {
        &self.overlay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::image::annotations::Role;
    use crate::image::parse::{ImageLayer, ImageMeta, TreadmillImage};

    fn three_layer() -> BackingChain {
        BackingChain::new(
            vec![
                PathBuf::from("/store/treadmill/img/blobs/sha256/base"),
                PathBuf::from("/store/treadmill/img/blobs/sha256/mid"),
                PathBuf::from("/store/treadmill/img/blobs/sha256/head"),
            ],
            "/run/treadmill/jobs/abc/overlay.qcow2",
        )
    }

    #[test]
    fn blockdev_three_layer_chain() {
        let expected = [
            "driver=file,node-name=tml-lower-0-file,filename=/store/treadmill/img/blobs/sha256/base,read-only=on",
            "driver=qcow2,node-name=tml-lower-0,file=tml-lower-0-file,read-only=on",
            "driver=file,node-name=tml-lower-1-file,filename=/store/treadmill/img/blobs/sha256/mid,read-only=on",
            "driver=qcow2,node-name=tml-lower-1,file=tml-lower-1-file,read-only=on,backing=tml-lower-0",
            "driver=file,node-name=tml-lower-2-file,filename=/store/treadmill/img/blobs/sha256/head,read-only=on",
            "driver=qcow2,node-name=tml-lower-2,file=tml-lower-2-file,read-only=on,backing=tml-lower-1",
            "driver=file,node-name=tml-disk-file,filename=/run/treadmill/jobs/abc/overlay.qcow2",
            "driver=qcow2,node-name=tml-disk,file=tml-disk-file,backing=tml-lower-2",
        ]
        .join("\n");
        assert_eq!(three_layer().blockdev_args().join("\n"), expected);
    }

    #[test]
    fn top_node_is_writable_and_references_head() {
        let chain = three_layer();
        let args = chain.blockdev_args();
        let top = args.last().unwrap();
        assert!(top.contains(&format!("node-name={}", chain.top_node())));
        // The writable top must not be read-only and must back onto the head.
        assert!(
            !top.contains("read-only=on"),
            "top overlay must be writable"
        );
        assert!(
            top.contains("backing=tml-lower-2"),
            "top must back onto head"
        );
    }

    #[test]
    fn all_lowers_are_read_only() {
        let chain = three_layer();
        for arg in chain.blockdev_args() {
            if arg.contains("node-name=tml-lower-") {
                assert!(arg.contains("read-only=on"), "lower not read-only: {arg}");
            }
        }
    }

    #[test]
    fn base_has_no_backing() {
        let args = three_layer().blockdev_args();
        // The base qcow2 node (tml-lower-0, not the -file node) names no backing.
        let base = args
            .iter()
            .find(|a| a.contains("node-name=tml-lower-0") && a.contains("driver=qcow2"))
            .unwrap();
        assert!(
            !base.contains("backing="),
            "base must have no backing: {base}"
        );
    }

    #[test]
    fn single_layer_overlay_backs_the_only_lower() {
        let chain = BackingChain::new(vec![PathBuf::from("/store/base")], "/run/job/overlay.qcow2");
        let args = chain.blockdev_args();
        // base (no backing) + writable top backing onto it: two nodes, two files.
        let base = args
            .iter()
            .find(|a| a.contains("node-name=tml-lower-0,"))
            .unwrap();
        assert!(!base.contains("backing="), "base must have no backing");
        let top = args.last().unwrap();
        assert!(
            top.contains("backing=tml-lower-0"),
            "top backs the only lower"
        );
    }

    #[test]
    fn a_prefix_keeps_two_chains_apart_in_one_node_graph() {
        let expected = [
            "driver=file,node-name=tml-bootfs-lower-0-file,filename=/store/blobs/sha256/boot,read-only=on",
            "driver=qcow2,node-name=tml-bootfs-lower-0,file=tml-bootfs-lower-0-file,read-only=on",
            "driver=file,node-name=tml-bootfs-disk-file,filename=/run/job/bootfs.qcow2",
            "driver=qcow2,node-name=tml-bootfs-disk,file=tml-bootfs-disk-file,backing=tml-bootfs-lower-0",
        ]
        .join("\n");
        let chain = BackingChain::with_prefix(
            "tml-bootfs",
            vec![PathBuf::from("/store/blobs/sha256/boot")],
            "/run/job/bootfs.qcow2",
        );
        assert_eq!(chain.blockdev_args().join("\n"), expected);
        assert_eq!(chain.top_node(), "tml-bootfs-disk");
        assert_eq!(BackingChain::new(vec![], "/o").top_node(), "tml-disk");
    }

    fn digest(n: u8) -> Digest {
        Digest::from_sha256([n; 32])
    }

    #[test]
    fn an_image_chain_opens_a_raw_base_with_the_raw_driver() {
        let role: Role = "disk".parse().unwrap();
        let image = TreadmillImage::new(
            vec![
                ImageLayer {
                    digest: digest(1),
                    size: 1024,
                    format: LayerFormat::Raw,
                    role: None,
                },
                ImageLayer {
                    digest: digest(2),
                    size: 10,
                    format: LayerFormat::Qcow2 {
                        virtual_size: 2048,
                        lower: Some(digest(1)),
                    },
                    role: Some(role),
                },
            ],
            ImageMeta::default(),
        )
        .unwrap();

        let chain = BackingChain::from_chain(
            "tml",
            &image.chain("disk").unwrap(),
            |d| PathBuf::from(format!("/store/{}", d.hex())),
            "/run/job/disk.qcow2",
        )
        .unwrap();

        let (base, head) = (digest(1).hex(), digest(2).hex());
        let expected = [
            format!("driver=file,node-name=tml-lower-0-file,filename=/store/{base},read-only=on"),
            "driver=raw,node-name=tml-lower-0,file=tml-lower-0-file,read-only=on".to_string(),
            format!("driver=file,node-name=tml-lower-1-file,filename=/store/{head},read-only=on"),
            "driver=qcow2,node-name=tml-lower-1,file=tml-lower-1-file,read-only=on,backing=tml-lower-0"
                .to_string(),
            "driver=file,node-name=tml-disk-file,filename=/run/job/disk.qcow2".to_string(),
            "driver=qcow2,node-name=tml-disk,file=tml-disk-file,backing=tml-lower-1".to_string(),
        ];
        assert_eq!(chain.blockdev_args(), expected);
    }

    #[test]
    fn an_image_chain_of_an_unknown_format_is_unsupported() {
        let image = TreadmillImage::new(
            vec![ImageLayer {
                digest: digest(1),
                size: 10,
                format: LayerFormat::Unknown {
                    media_type: "application/vnd.example.future".to_string(),
                },
                role: Some("disk".parse().unwrap()),
            }],
            ImageMeta::default(),
        )
        .unwrap();

        assert_eq!(
            BackingChain::from_chain(
                "tml",
                &image.chain("disk").unwrap(),
                |_| PathBuf::from("/store/x"),
                "/run/job/disk.qcow2",
            ),
            Err(UnsupportedLayer {
                digest: digest(1),
                media_type: "application/vnd.example.future".to_string(),
            }),
        );
    }
}
