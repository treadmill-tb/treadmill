//! Validate the backing-chain emitter against real qemu (Tier-2, plan §12.5).
//!
//! [`BackingChain::blockdev_args`] is fed to `qemu-storage-daemon`, which
//! assembles the qcow2 node graph and exports the writable top over NBD; we then
//! read through that export with `qemu-io` and assert the chain composes in the
//! right order (an upper layer shadows a lower one; an untouched region falls
//! through to the base) and that the top is writable. This is the same
//! `-blockdev` graph `qemu-system-*` consumes, so it covers both runtimes
//! without booting; the Phase 2 boot test then exercises the whole path once.
//!
//! (This supersedes the plan's `qemu-nbd --image-opts` check: `--image-opts`
//! cannot express an inline backing node, so an unbaked multi-layer chain only
//! goes through the node graph — see `treadmill_rs::image::blockdev`.)
//!
//! Skips when the qemu tools are not on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use treadmill_rs::image::blockdev::BackingChain;

const VIRTUAL_SIZE: u64 = 16 * 1024 * 1024;

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|c| c.is_file())
}

struct Tools {
    storage_daemon: PathBuf,
    qemu_img: PathBuf,
    qemu_io: PathBuf,
}

fn tools() -> Option<Tools> {
    Some(Tools {
        storage_daemon: which("qemu-storage-daemon")?,
        qemu_img: which("qemu-img")?,
        qemu_io: which("qemu-io")?,
    })
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("spawn qemu tool");
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}

/// `qemu-io` script against a file (used to seed layer content).
fn qemu_io_file(qemu_io: &Path, file: &Path, script: &[&str]) {
    let mut cmd = Command::new(qemu_io);
    cmd.arg("-f").arg("qcow2").arg(file);
    for c in script {
        cmd.arg("-c").arg(c);
    }
    run(&mut cmd);
}

/// A running storage daemon exporting the assembled chain over NBD; killed on drop.
struct StorageDaemon {
    child: Child,
    socket: PathBuf,
}

impl Drop for StorageDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl StorageDaemon {
    fn nbd_uri(&self) -> String {
        format!("nbd+unix:///disk?socket={}", self.socket.display())
    }

    fn start(sd: &Path, chain: &BackingChain, socket: PathBuf) -> StorageDaemon {
        let mut cmd = Command::new(sd);
        for node in chain.blockdev_args() {
            cmd.arg("--blockdev").arg(node);
        }
        cmd.arg("--nbd-server")
            .arg(format!("addr.type=unix,addr.path={}", socket.display()))
            .arg("--export")
            .arg(format!(
                "type=nbd,id=e0,node-name={},name=disk,writable=on",
                BackingChain::TOP_NODE,
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn qemu-storage-daemon");

        let daemon = StorageDaemon { child, socket };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !daemon.socket.exists() {
            assert!(
                Instant::now() < deadline,
                "storage daemon socket never appeared"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // The socket file can exist a hair before the listener accepts; settle.
        std::thread::sleep(Duration::from_millis(100));
        daemon
    }
}

#[test]
fn blockdev_chain_composes_and_is_writable() {
    let Some(t) = tools() else {
        eprintln!("qemu-storage-daemon/qemu-img/qemu-io not on PATH; skipping");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.qcow2");
    let head = dir.path().join("head.qcow2");
    let overlay = dir.path().join("overlay.qcow2");
    for f in [&base, &head, &overlay] {
        run(Command::new(&t.qemu_img)
            .args(["create", "-f", "qcow2"])
            .arg(f)
            .arg(VIRTUAL_SIZE.to_string()));
    }

    // base: 0xBB at offset 0, 0xDD at 1 MiB.  head: 0xAA at offset 0 (shadows
    // base) and nothing at 1 MiB (so the base shows through).
    qemu_io_file(
        &t.qemu_io,
        &base,
        &["write -P 0xBB 0 4096", "write -P 0xDD 1M 4096"],
    );
    qemu_io_file(&t.qemu_io, &head, &["write -P 0xAA 0 4096"]);

    let chain = BackingChain::new(vec![base.clone(), head.clone()], overlay.clone());
    let daemon = StorageDaemon::start(&t.storage_daemon, &chain, dir.path().join("nbd.sock"));

    // Read through the assembled export and verify composition + writability.
    let read = |script: &[&str]| -> String {
        let mut cmd = Command::new(&t.qemu_io);
        cmd.arg("-f").arg("raw").arg(daemon.nbd_uri());
        for c in script {
            cmd.arg("-c").arg(c);
        }
        let out = cmd.output().expect("qemu-io over nbd");
        assert!(
            out.status.success(),
            "qemu-io failed: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // `read -P` verifies the bytes match the pattern; a mismatch prints a
    // failure line (so assert none appears) — head shadows base at 0, base shows
    // through at 1 MiB.
    let verify = read(&["read -P 0xAA 0 4096", "read -P 0xDD 1M 4096"]);
    assert!(
        !verify.to_lowercase().contains("verification failed"),
        "chain did not compose as overlay->head->base: {verify}",
    );

    // The exported top is writable: a write lands (in the overlay) and reads back.
    let rw = read(&["write -P 0xCC 2M 4096", "read -P 0xCC 2M 4096"]);
    assert!(
        !rw.to_lowercase().contains("verification failed"),
        "writable top did not retain its write: {rw}",
    );
}
