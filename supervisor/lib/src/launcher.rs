//! Injectable seam for the supervisors' subprocess operations.
//!
//! The QEMU and NBD-netboot supervisors drive a small set of external
//! processes: `qemu-img` to inspect and allocate qcow2 images, and the workload
//! itself (`qemu-system-*` / `qemu-nbd`). Funnelling those through the
//! [`ProcessLauncher`] trait lets the job state machine be driven in-process by
//! tests with a stub launcher — observing the arguments it would have run and
//! simulating workload exit — without spawning real binaries. See
//! `doc/oci-image-migration-plan.md` §12 (Phase 0.5).

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

/// Partial metadata of a qcow2 image, as reported by `qemu-img info`.
///
/// Only the fields the supervisors validate are modelled; unknown fields are
/// ignored. Kept here (next to the launcher that produces it) so a stub launcher
/// can construct it directly in tests.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct QemuImgMetadata {
    pub filename: PathBuf,
    pub virtual_size: u64,
    pub children: Vec<QemuImgChildMetadata>,
    pub encrypted: Option<bool>,
    pub backing_filename_format: Option<String>,
    // According to [1], these attributes are described as
    // - backing-filename: name of the backing file
    // - full-backing-filename: full path of the backing file
    //
    // In practice it seems that `full-backing-filename` points to the resolved
    // path of the backing file (relative to the current working directory),
    // whereas `backing-filename` is just the raw attribute stored in the image.
    //
    // [1]: https://www.qemu.org/docs/master/interop/qemu-storage-daemon-qmp-ref.html
    pub backing_filename: Option<PathBuf>,
    pub full_backing_filename: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct QemuImgChildMetadata {
    pub info: QemuImgChildMetadataInfo,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct QemuImgChildMetadataInfo {
    // We're only interested in the filename here, to make sure that the image
    // has only one child node, and that node coincides with the file that we're
    // operating on.
    pub filename: PathBuf,

    // Include this field just to make sure that we don't have any recursive
    // children:
    pub children: Vec<QemuImgChildMetadata>,
}

/// A spawned workload process the job state machine waits on and can kill.
///
/// Implemented by [`ChildProcess`] over a real [`tokio::process::Child`]; a
/// stub launcher can implement it over a channel to simulate process lifetime.
#[async_trait]
pub trait WorkloadProcess: Send {
    /// Wait for the process to exit.
    async fn wait(&mut self) -> std::io::Result<ExitStatus>;

    /// Forcibly terminate the process.
    async fn kill(&mut self) -> std::io::Result<()>;
}

/// The subprocess operations a supervisor's job state machine performs.
///
/// Production code uses [`CliLauncher`]; tests inject a stub to drive the state
/// machine without real binaries.
#[async_trait]
pub trait ProcessLauncher: std::fmt::Debug + Send + Sync {
    /// Read partial metadata of a qcow2 image (`qemu-img info`).
    async fn qcow2_info(&self, image: &Path) -> Result<QemuImgMetadata>;

    /// Create a thin-provisioned qcow2 overlay of `virtual_size_bytes`, backed
    /// by `backing` (`qemu-img create`).
    async fn create_overlay(
        &self,
        disk: &Path,
        backing: &Path,
        virtual_size_bytes: u64,
    ) -> Result<()>;

    /// Spawn a workload process: `program` with `args`, stdin closed and
    /// stdout/stderr inherited.
    async fn spawn(&self, program: &Path, args: &[String]) -> Result<Box<dyn WorkloadProcess>>;
}

/// Wraps a real [`tokio::process::Child`] as a [`WorkloadProcess`].
pub struct ChildProcess(pub tokio::process::Child);

#[async_trait]
impl WorkloadProcess for ChildProcess {
    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.0.wait().await
    }

    async fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill().await
    }
}

/// [`ProcessLauncher`] backed by the real `qemu-img` binary and `tokio` process
/// spawning.
#[derive(Debug, Clone)]
pub struct CliLauncher {
    qemu_img_binary: PathBuf,
}

impl CliLauncher {
    pub fn new(qemu_img_binary: impl Into<PathBuf>) -> Self {
        CliLauncher {
            qemu_img_binary: qemu_img_binary.into(),
        }
    }
}

#[async_trait]
impl ProcessLauncher for CliLauncher {
    async fn qcow2_info(&self, image: &Path) -> Result<QemuImgMetadata> {
        let metadata_output = tokio::process::Command::new(&self.qemu_img_binary)
            .arg("info")
            .arg("--format=qcow2")
            .arg("--output=json")
            .arg("--")
            .arg(image)
            .output()
            .await
            .map_err(anyhow::Error::from)
            .and_then(|output| {
                // Ideally we'd want to use the nightly `exit_ok()` here:
                if !output.status.success() {
                    bail!(
                        "Running qemu-img failed with exit-code {:?}",
                        output.status.code()
                    );
                }

                // Don't care about stderr:
                Ok(output.stdout)
            })
            .with_context(|| format!("Failed to query image metadata for {image:?}"))?;

        serde_json::from_slice(&metadata_output).with_context(|| {
            format!(
                "Failed to parse qemu-img info output for {:?}: {:?}",
                image,
                String::from_utf8_lossy(&metadata_output),
            )
        })
    }

    async fn create_overlay(
        &self,
        disk: &Path,
        backing: &Path,
        virtual_size_bytes: u64,
    ) -> Result<()> {
        tokio::process::Command::new(&self.qemu_img_binary)
            .arg("create")
            // Image format:
            .arg("-f")
            .arg("qcow2")
            // Backing file format:
            .arg("-F")
            .arg("qcow2")
            // Backing file:
            .arg("-b")
            .arg(backing)
            // New image file:
            .arg(disk)
            // New image size (in bytes, without suffix):
            .arg(virtual_size_bytes.to_string())
            .output()
            .await
            .map_err(anyhow::Error::from)
            .and_then(|output| {
                // Ideally we'd want to use the nightly `exit_ok()` here:
                if !output.status.success() {
                    bail!(
                        "Running qemu-img failed with exit-code {:?}, stdout: {:?}, stderr: {:?}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }

                // Don't care about stdout or stderr in the success case:
                Ok(())
            })
    }

    async fn spawn(&self, program: &Path, args: &[String]) -> Result<Box<dyn WorkloadProcess>> {
        let child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn workload process {program:?}"))?;

        Ok(Box::new(ChildProcess(child)))
    }
}
