//! Attaching a qcow2 image as a block device.
//!
//! Provisioning needs a real file system, which means a real block device, and
//! `qemu-nbd` is what turns a qcow2 chain into one. Everything here needs
//! root, so it all goes through [`Privilege`].
//!
//! Two details are load-bearing and easy to lose. Attaching asks for
//! `--discard=unmap --detect-zeroes=unmap`, without which the `fstrim` at the
//! end of a build frees nothing in the shipped blob. Detaching waits for the
//! server process to actually exit, because `--disconnect` returns before it
//! does and reusing the device too early corrupts the next attachment.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::exec::{self, Privilege};

/// How long to wait for a partition table to show up, and for a server to go.
const SETTLE_INTERVAL: Duration = Duration::from_millis(100);
const PARTITION_ATTEMPTS: u32 = 50;
const EXIT_ATTEMPTS: u32 = 100;

/// An attached device.
///
/// Holding one does not keep the attachment alive — [`crate::cleanup`] owns
/// that — it just says where it is.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub device: PathBuf,
    pub pid_file: PathBuf,
}

impl Attachment {
    /// Partition `number` of this device, as the kernel names it.
    pub fn partition(&self, number: u32) -> PathBuf {
        let mut name = self.device.clone().into_os_string();
        name.push(format!("p{number}"));
        PathBuf::from(name)
    }

    /// The partitions the kernel found, in order.
    pub fn partitions(&self) -> Result<Vec<PathBuf>> {
        partitions_of(&self.device)
    }
}

/// Attach `image` to a free NBD device.
///
/// `partitioned` says whether to wait for the kernel to read a partition
/// table off it; for a bare file system there is none to wait for, and
/// waiting would just be a delay before the inevitable.
pub fn attach(
    privilege: Privilege,
    work: &Path,
    image: &Path,
    partitioned: bool,
) -> Result<Attachment> {
    ensure_module(privilege)?;

    let qemu_nbd = exec::which("qemu-nbd")
        // Resolved before escalating: sudo's secure_path is not the caller's
        // PATH, and a tool from a dev shell would not be found again as root.
        .context("qemu-nbd is not on PATH")?;

    for device in candidates()? {
        let name = device
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Claim the device before looking at it. The look-then-connect that
        // this replaces is a race: two concurrent builds can both see the
        // same device free.
        let _claim = match claim(&name) {
            Some(claim) => claim,
            None => continue,
        };
        if in_use(&device) {
            continue;
        }

        let pid_file = work.join(format!("{name}.pid"));
        let attached = exec::run(
            privilege
                .command(&qemu_nbd)
                .arg(format!("--connect={}", device.display()))
                .args(["--format=qcow2", "--discard=unmap", "--detect-zeroes=unmap"])
                .arg(format!("--pid-file={}", pid_file.display()))
                .arg(image)
                // A device that is busy in a way /sys did not show is an
                // ordinary outcome of the probe, not something to report.
                .stderr(std::process::Stdio::null()),
        );
        if attached.is_err() {
            continue;
        }

        let attachment = Attachment { device, pid_file };
        if partitioned {
            await_partitions(&attachment)?;
        }
        return Ok(attachment);
    }

    bail!("found no free NBD device for {}", image.display())
}

/// Detach a device and wait for its server to go.
pub fn detach(privilege: Privilege, device: &Path, pid_file: &Path) -> Result<()> {
    let qemu_nbd = exec::which("qemu-nbd").context("qemu-nbd is not on PATH")?;

    // The server writes its pid file as root, so reading it needs root too.
    let pid = exec::capture(privilege.command("cat").arg(pid_file))
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok());

    exec::run(
        privilege
            .command(&qemu_nbd)
            .arg("--disconnect")
            .arg(device)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )
    .with_context(|| format!("detach {}", device.display()))?;

    let Some(pid) = pid else {
        return Ok(());
    };
    // --disconnect returns before the server exits. Reusing the device in
    // that window corrupts the next attachment, so this wait is not optional.
    for _ in 0..EXIT_ATTEMPTS {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = crate::cleanup::remove_if_present(pid_file);
            return Ok(());
        }
        std::thread::sleep(SETTLE_INTERVAL);
    }
    bail!("the qemu-nbd serving {} did not exit", device.display())
}

/// Load the `nbd` module if it is not loaded.
///
/// `max_part` has to be set at load time; without it the kernel reads no
/// partition table and a partitioned image attaches as one opaque device.
fn ensure_module(privilege: Privilege) -> Result<()> {
    if Path::new("/sys/module/nbd").exists() {
        return Ok(());
    }
    exec::run(privilege.command("modprobe").args(["nbd", "max_part=16"]))
        .context("load the nbd kernel module")
}

/// Every `/dev/nbd*` the kernel offers, in index order.
fn candidates() -> Result<Vec<PathBuf>> {
    let mut devices: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir("/sys/block").context("read /sys/block")? {
        let entry = entry.context("read /sys/block")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = name.strip_prefix("nbd").and_then(|i| i.parse().ok()) else {
            continue;
        };
        devices.push((index, PathBuf::from("/dev").join(&name)));
    }
    devices.sort();
    Ok(devices.into_iter().map(|(_, device)| device).collect())
}

/// Whether a device already has a server behind it.
fn in_use(device: &Path) -> bool {
    let Some(name) = device.file_name() else {
        return true;
    };
    Path::new("/sys/block").join(name).join("pid").exists()
}

/// An advisory claim on one device, held while it is probed and connected.
///
/// Released once the server is up, by which point `/sys/block/<dev>/pid`
/// makes the device visibly busy to everyone else.
struct Claim(fs::File);

fn claim(name: &str) -> Option<Claim> {
    use std::os::unix::io::AsRawFd;

    let path = std::env::temp_dir().join(format!(".tml-{name}.lock"));
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    // Advisory and non-blocking: a device another build is claiming is one
    // this build should move past, not wait for.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (locked == 0).then_some(Claim(file))
}

impl Drop for Claim {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Wait for the kernel to publish the device's partitions.
fn await_partitions(attachment: &Attachment) -> Result<()> {
    for _ in 0..PARTITION_ATTEMPTS {
        if attachment.partition(1).exists() {
            return Ok(());
        }
        std::thread::sleep(SETTLE_INTERVAL);
    }
    bail!(
        "{} shows no partitions; is the nbd module loaded with max_part > 0?",
        attachment.device.display(),
    )
}

/// The partition devices of `device`, in order.
pub fn partitions_of(device: &Path) -> Result<Vec<PathBuf>> {
    let Some(name) = device.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{name}p");
    let parent = device.parent().unwrap_or(Path::new("/dev"));

    let mut partitions: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(parent).with_context(|| format!("read {}", parent.display()))? {
        let entry = entry.with_context(|| format!("read {}", parent.display()))?;
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = entry_name
            .strip_prefix(&prefix)
            .and_then(|i| i.parse().ok())
        else {
            continue;
        };
        let path = entry.path();
        let is_block = fs::metadata(&path)
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false);
        if is_block {
            partitions.push((index, path));
        }
    }
    partitions.sort();
    Ok(partitions.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partition_is_the_device_plus_p_and_a_number() {
        let attachment = Attachment {
            device: PathBuf::from("/dev/nbd3"),
            pid_file: PathBuf::from("/tmp/nbd3.pid"),
        };
        assert_eq!(attachment.partition(1), Path::new("/dev/nbd3p1"));
        assert_eq!(attachment.partition(12), Path::new("/dev/nbd3p12"));
    }

    #[test]
    fn a_claim_excludes_a_second_one() {
        let name = "test-claim-exclusion";
        let first = claim(name);
        assert!(first.is_some(), "the first claim should succeed");
        assert!(claim(name).is_none(), "a second claim must not");
        drop(first);
        assert!(claim(name).is_some(), "and it is released on drop");
    }
}
