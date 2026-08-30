//! Console-log capture plumbing shared by the supervisors.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

use crate::launcher::BoxedAsyncRead;

/// How long to wait for a process (e.g., QEMU) to connect back to the serial
/// socket before giving up on the `serial` channel.
///
/// qemu connects within milliseconds of launch. This value exists to
/// deliberately fail if no process binds, instead of silently hanging the task.
const SERIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A unix-domain socket the supervisor listens on for the job's serial console
/// (to then be attached to later, e.g. by QEMU).
///
/// The supervisor binds the listener before launching the job. For QEMU, it
/// will then passes qemu a matching `-chardev socket,...,server=off` (QEMU
/// connects as the client). After launch, [`accept`](SerialSocket::accept)
/// yields the single qemu connection as a readable byte stream, fed into the `serial`
/// log channel.
///
/// TODO: how much of this is actually specific to the QEMU supervisor, and
/// should be moved there?
#[derive(Debug)]
pub struct SerialSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl SerialSocket {
    /// Bind a fresh serial socket at `path`, removing any stale socket file
    /// left behind by a prior run first.
    pub async fn bind(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        // A leftover socket file from a previous (crashed) run would make
        // `bind` fail with `AddrInUse`; clear it. Ignore "not found".
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        Ok(SerialSocket { listener, path })
    }

    /// Filesystem path of the bound socket.
    ///
    /// Embed this in qemu's `-chardev socket,...,path=<...>` argument.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept qemu's connection to the serial socket, with a timeout.
    ///
    /// Consumes the listener: a serial console is a single connection. The
    /// returned [`UnixStream`] is the readable `serial` channel.
    pub async fn accept(self) -> std::io::Result<UnixStream> {
        let (stream, _addr) = tokio::time::timeout(SERIAL_CONNECT_TIMEOUT, self.listener.accept())
            .await
            .map_err(|_elapsed| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "qemu did not connect to the serial socket within the timeout",
                )
            })??;
        Ok(stream)
    }
}

/// Fallback consumer of the captured channels.
///
/// Drains each present channel into the supervisor's own stdout/stderr so the
/// operator keeps seeing console output and qemu's stdout/stderr pipes never
/// fill (an undrained pipe would block qemu). The serial connection is accepted
/// lazily inside the spawned task so the caller is not blocked waiting on qemu.
///
/// TODO: this is probably never something we actually want. Instead, logs
/// should be written to a local file, or entirely discarded, when no log
/// streaming option is present. The job's output shouldn't be able to pollute
/// the host's journal. Refactor accordingly.
pub fn drain_to_stdio(
    stdout: Option<BoxedAsyncRead>,
    stderr: Option<BoxedAsyncRead>,
    serial: Option<SerialSocket>,
) {
    if let Some(mut reader) = stdout {
        tokio::spawn(async move {
            let mut sink = tokio::io::stdout();
            if let Err(e) = tokio::io::copy(&mut reader, &mut sink).await {
                tracing::warn!(error = ?e, "qemu-stdout capture drain ended with error");
            }
        });
    }
    if let Some(mut reader) = stderr {
        tokio::spawn(async move {
            let mut sink = tokio::io::stderr();
            if let Err(e) = tokio::io::copy(&mut reader, &mut sink).await {
                tracing::warn!(error = ?e, "qemu-stderr capture drain ended with error");
            }
        });
    }
    if let Some(socket) = serial {
        tokio::spawn(async move {
            match socket.accept().await {
                Ok(mut stream) => {
                    let mut sink = tokio::io::stdout();
                    if let Err(e) = tokio::io::copy(&mut stream, &mut sink).await {
                        tracing::warn!(error = ?e, "serial capture drain ended with error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to accept qemu serial connection");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The serial socket round-trips bytes from a connecting client (standing in
    /// for qemu) to the accepted reader.
    #[tokio::test]
    async fn serial_socket_round_trips_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("serial.sock");

        let socket = SerialSocket::bind(&sock_path).await.expect("bind");
        assert_eq!(socket.path(), sock_path.as_path());

        // Connect as qemu would (client side) and write the "serial output".
        let client_path = sock_path.clone();
        let writer = tokio::spawn(async move {
            let mut client = UnixStream::connect(&client_path).await.expect("connect");
            client.write_all(b"serial-bytes").await.expect("write");
            client.shutdown().await.expect("shutdown");
        });

        let mut stream = socket.accept().await.expect("accept");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        assert_eq!(buf, b"serial-bytes");

        writer.await.unwrap();
    }

    /// `bind` succeeds even when a stale socket file is already present at the
    /// path (e.g. left behind by a crashed prior run).
    #[tokio::test]
    async fn serial_socket_bind_clears_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("serial.sock");

        let first = SerialSocket::bind(&sock_path).await.expect("first bind");
        drop(first); // leaves the socket file on disk
        assert!(sock_path.exists());

        // Rebinding the same path must not fail with AddrInUse.
        SerialSocket::bind(&sock_path).await.expect("rebind");
    }
}
