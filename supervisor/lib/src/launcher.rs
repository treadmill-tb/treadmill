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
use tokio::io::AsyncRead;

/// A captured byte stream of a workload's stdout or stderr.
///
/// Boxed and type-erased so the launcher can hand the consumer a uniform reader
/// regardless of the concrete process implementation (a real
/// [`tokio::process::ChildStdout`]/`ChildStderr`, or an in-memory cursor in
/// tests).
pub type BoxedAsyncRead = Box<dyn AsyncRead + Send + Unpin>;

/// How a spawned workload's stdout/stderr are wired up.
///
/// Log streaming (see `doc/log-streaming-plan.md`) needs qemu's stdout/stderr as
/// readable byte streams; when streaming is disabled we keep the historical
/// behavior of inheriting the supervisor's own fds so the operator sees output
/// on the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioMode {
    /// stdout/stderr inherit the supervisor's fds (no capture). This is the
    /// behavior used when log streaming is disabled.
    Inherit,
    /// stdout/stderr are piped and exposed as [`BoxedAsyncRead`] via
    /// [`WorkloadProcess::take_stdout`] / [`WorkloadProcess::take_stderr`].
    Capture,
}

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

    /// Take ownership of the process's captured stdout, if it was spawned with
    /// [`StdioMode::Capture`].
    ///
    /// Returns `None` if stdout was inherited (not captured) or has already been
    /// taken. The default implementation returns `None`, so process stubs that
    /// don't model capture need not override it.
    fn take_stdout(&mut self) -> Option<BoxedAsyncRead> {
        None
    }

    /// Take ownership of the process's captured stderr. See [`take_stdout`].
    ///
    /// [`take_stdout`]: WorkloadProcess::take_stdout
    fn take_stderr(&mut self) -> Option<BoxedAsyncRead> {
        None
    }
}

/// The subprocess operations a supervisor's job state machine performs.
///
/// Production code uses [`CliLauncher`]; tests inject a stub to drive the state
/// machine without real binaries.
#[async_trait]
pub trait ProcessLauncher: std::fmt::Debug + Send + Sync {
    /// Read partial metadata of a qcow2 image (`qemu-img info`).
    async fn qcow2_info(&self, image: &Path) -> Result<QemuImgMetadata>;

    /// Create a thin-provisioned qcow2 overlay of `virtual_size_bytes` with
    /// **no baked backing file** (`qemu-img create -f qcow2 <disk> <size>`).
    ///
    /// Treadmill never bakes a backing path into a per-job overlay: the backing
    /// chain is supplied at launch via `-blockdev` nodes (D3/D9, see
    /// [`treadmill_rs::image::blockdev::BackingChain`]).
    async fn create_overlay_no_backing(&self, disk: &Path, virtual_size_bytes: u64) -> Result<()>;

    /// Spawn a workload process: `program` with `args` (optionally in working
    /// directory `cwd`), stdin closed and stdout/stderr wired up per `stdio`
    /// ([`StdioMode::Inherit`] to inherit the supervisor's fds, or
    /// [`StdioMode::Capture`] to pipe them for retrieval via
    /// [`WorkloadProcess::take_stdout`] / [`WorkloadProcess::take_stderr`]).
    async fn spawn(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        stdio: StdioMode,
    ) -> Result<Box<dyn WorkloadProcess>>;
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

    fn take_stdout(&mut self) -> Option<BoxedAsyncRead> {
        self.0
            .stdout
            .take()
            .map(|stdout| Box::new(stdout) as BoxedAsyncRead)
    }

    fn take_stderr(&mut self) -> Option<BoxedAsyncRead> {
        self.0
            .stderr
            .take()
            .map(|stderr| Box::new(stderr) as BoxedAsyncRead)
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

    async fn create_overlay_no_backing(&self, disk: &Path, virtual_size_bytes: u64) -> Result<()> {
        tokio::process::Command::new(&self.qemu_img_binary)
            .arg("create")
            // Image format:
            .arg("-f")
            .arg("qcow2")
            // New image file (no `-b`/`-F`: the backing chain is supplied at
            // launch via -blockdev nodes, never baked here):
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

    async fn spawn(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        stdio: StdioMode,
    ) -> Result<Box<dyn WorkloadProcess>> {
        // `Capture` pipes stdout/stderr so the caller can read them as
        // `AsyncRead` (for log streaming); `Inherit` keeps the historical
        // terminal-inheriting behavior used when streaming is disabled.
        let (stdout, stderr) = match stdio {
            StdioMode::Inherit => (
                std::process::Stdio::inherit(),
                std::process::Stdio::inherit(),
            ),
            StdioMode::Capture => (std::process::Stdio::piped(), std::process::Stdio::piped()),
        };

        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let child = command
            .spawn()
            .with_context(|| format!("Failed to spawn workload process {program:?}"))?;

        Ok(Box::new(ChildProcess(child)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    /// A process stub that hands out in-memory readers as its captured streams —
    /// enough to exercise the [`WorkloadProcess`] capture surface (and, in later
    /// phases, the log publisher) without spawning a real binary.
    struct StubCaptureProcess {
        stdout: Option<BoxedAsyncRead>,
        stderr: Option<BoxedAsyncRead>,
    }

    impl StubCaptureProcess {
        fn new(stdout: &[u8], stderr: &[u8]) -> Self {
            StubCaptureProcess {
                stdout: Some(Box::new(Cursor::new(stdout.to_vec()))),
                stderr: Some(Box::new(Cursor::new(stderr.to_vec()))),
            }
        }
    }

    #[async_trait]
    impl WorkloadProcess for StubCaptureProcess {
        async fn wait(&mut self) -> std::io::Result<ExitStatus> {
            std::future::pending().await
        }
        async fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn take_stdout(&mut self) -> Option<BoxedAsyncRead> {
            self.stdout.take()
        }
        fn take_stderr(&mut self) -> Option<BoxedAsyncRead> {
            self.stderr.take()
        }
    }

    async fn read_to_vec(mut reader: BoxedAsyncRead) -> Vec<u8> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.expect("read captured");
        buf
    }

    #[tokio::test]
    async fn stub_process_captured_streams_are_readable() {
        let mut proc = StubCaptureProcess::new(b"hello stdout", b"hello stderr");

        let stdout = proc.take_stdout().expect("stdout present");
        let stderr = proc.take_stderr().expect("stderr present");
        assert_eq!(read_to_vec(stdout).await, b"hello stdout");
        assert_eq!(read_to_vec(stderr).await, b"hello stderr");

        // A stream may only be taken once.
        assert!(proc.take_stdout().is_none());
        assert!(proc.take_stderr().is_none());
    }

    /// `StdioMode::Capture` must actually pipe a real subprocess's stdout/stderr
    /// back to the caller as readable `AsyncRead` streams.
    #[tokio::test]
    async fn cli_launcher_capture_pipes_real_subprocess_output() {
        // `qemu-img` is irrelevant here; spawn a tiny shell command instead.
        let launcher = CliLauncher::new("/nonexistent/qemu-img");
        let mut proc = launcher
            .spawn(
                Path::new("sh"),
                &[
                    "-c".to_string(),
                    "printf 'out-bytes'; printf 'err-bytes' >&2".to_string(),
                ],
                None,
                StdioMode::Capture,
            )
            .await
            .expect("spawn sh");

        let stdout = proc.take_stdout().expect("stdout captured");
        let stderr = proc.take_stderr().expect("stderr captured");
        assert_eq!(read_to_vec(stdout).await, b"out-bytes");
        assert_eq!(read_to_vec(stderr).await, b"err-bytes");

        proc.wait().await.expect("wait sh");
    }

    /// `StdioMode::Inherit` does not capture: there are no streams to take.
    #[tokio::test]
    async fn cli_launcher_inherit_does_not_capture() {
        let launcher = CliLauncher::new("/nonexistent/qemu-img");
        let mut proc = launcher
            .spawn(
                Path::new("sh"),
                &["-c".to_string(), "true".to_string()],
                None,
                StdioMode::Inherit,
            )
            .await
            .expect("spawn sh");

        assert!(proc.take_stdout().is_none());
        assert!(proc.take_stderr().is_none());
        proc.wait().await.expect("wait sh");
    }
}
