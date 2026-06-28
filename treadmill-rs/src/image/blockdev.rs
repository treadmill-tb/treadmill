//! Runtime assembly of a qcow2 backing chain for `qemu-system-*` and
//! `qemu-storage-daemon`.
//!
//! Treadmill never bakes backing paths into the shared qcow2 blobs (D3): the
//! chain is supplied at launch. This module turns an ordered chain — shared
//! read-only lower layers (base first) plus the per-job writable overlay on top
//! — into a `-blockdev` node graph ([`BackingChain::blockdev_args`]): each layer
//! is a `file` node feeding a `qcow2` node, lowers pinned `read-only=on`, the
//! overlay writable, exposed at [`BackingChain::TOP_NODE`].
//!
//! Both supervisors share **one** form — the `-blockdev` node graph:
//!
//! - `qemu-system-*` takes the nodes directly and attaches its device to
//!   [`BackingChain::TOP_NODE`];
//! - the netboot path feeds the *same* nodes to `qemu-storage-daemon` and adds
//!   an NBD `--export` of [`BackingChain::TOP_NODE`].
//!
//! This supersedes the original plan's `qemu-nbd --image-opts` form (D9):
//! `qemu-nbd`/`qemu-img` `--image-opts` is a single `QemuOpts` blockdev and
//! **cannot express an inline backing node** (`qcow2` rejects `backing.driver`),
//! so an unbaked multi-layer chain (D3) is impossible to convey that way.
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
//! paths do not). See `doc/oci-image-migration-plan.md` §6.2/§D9.

use std::path::{Path, PathBuf};

/// A qcow2 backing chain ready to be handed to a runtime.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackingChain {
    /// Shared read-only lower layers, **base first … head last**.
    lowers: Vec<PathBuf>,
    /// The per-job writable overlay layered on top of the head (created with no
    /// baked backing).
    overlay: PathBuf,
}

impl BackingChain {
    /// `node-name` of the top (writable) qcow2 node a `-blockdev` graph exposes;
    /// the qemu device's `drive=` should reference this.
    pub const TOP_NODE: &'static str = "tml-disk";

    /// Build a chain from the ordered shared lowers (base first) and the per-job
    /// writable overlay.
    pub fn new(lowers: Vec<PathBuf>, overlay: impl Into<PathBuf>) -> Self {
        BackingChain {
            lowers,
            overlay: overlay.into(),
        }
    }

    /// `node-name` of the i-th lower's qcow2 node.
    fn lower_node(i: usize) -> String {
        format!("tml-lower-{i}")
    }

    /// The `-blockdev` option strings (the values after each `-blockdev`), in
    /// dependency order so each node references one already defined. The caller
    /// passes each as its own `-blockdev <value>` and attaches the device to
    /// [`BackingChain::TOP_NODE`].
    pub fn blockdev_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity((self.lowers.len() + 1) * 2);

        for (i, path) in self.lowers.iter().enumerate() {
            let fmt_node = Self::lower_node(i);
            let file_node = format!("{fmt_node}-file");

            args.push(format!(
                "driver=file,node-name={file_node},filename={},read-only=on",
                path.display(),
            ));

            // The base (i == 0) is opened with no backing; every other lower
            // backs onto the one below it. All lowers are read-only and shared.
            let mut qcow2 =
                format!("driver=qcow2,node-name={fmt_node},file={file_node},read-only=on");
            if i > 0 {
                qcow2.push_str(&format!(",backing={}", Self::lower_node(i - 1)));
            }
            args.push(qcow2);
        }

        // The writable per-job overlay on top.
        let top_file = format!("{}-file", Self::TOP_NODE);
        args.push(format!(
            "driver=file,node-name={top_file},filename={}",
            self.overlay.display(),
        ));
        let mut top = format!("driver=qcow2,node-name={},file={top_file}", Self::TOP_NODE,);
        if let Some(last) = self.lowers.len().checked_sub(1) {
            top.push_str(&format!(",backing={}", Self::lower_node(last)));
        }
        args.push(top);

        args
    }

    /// The blob files the chain reads, base first (excludes the per-job overlay).
    pub fn lower_paths(&self) -> &[PathBuf] {
        &self.lowers
    }

    /// The per-job writable overlay path.
    pub fn overlay_path(&self) -> &Path {
        &self.overlay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let args = three_layer().blockdev_args();
        let top = args.last().unwrap();
        assert!(top.contains(&format!("node-name={}", BackingChain::TOP_NODE)));
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
}
