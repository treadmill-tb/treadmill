//! Console-log capture plumbing shared by the supervisors.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use treadmill_rs::api::switchboard_supervisor::LogChannel;

use crate::launcher::BoxedAsyncRead;

pub trait AsyncStream: AsyncRead + AsyncWrite {}

impl<T: AsyncRead + AsyncWrite> AsyncStream for T {}

pub type BoxedAsyncStream = Box<dyn AsyncStream + Send + Unpin>;

pub enum SerialConsole {
    Listener(SerialSocket),
    Stream(BoxedAsyncStream),
}

impl std::fmt::Debug for SerialConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SerialConsole::Listener(socket) => f.debug_tuple("Listener").field(socket).finish(),
            SerialConsole::Stream(_) => f.write_str("Stream"),
        }
    }
}

impl SerialConsole {
    pub async fn connect(self) -> std::io::Result<BoxedAsyncStream> {
        match self {
            SerialConsole::Listener(socket) => Ok(Box::new(socket.accept().await?)),
            SerialConsole::Stream(stream) => Ok(stream),
        }
    }

    /// Returns the console and a handle that closes it when dropped.
    ///
    /// A serial device never reaches EOF, so its reader would otherwise hold
    /// it open indefinitely, and the next job's exclusive open fails with
    /// `EBUSY`. A socket console is closed by its peer, and left as is.
    pub fn close_on_drop(self) -> (Self, oneshot::Sender<()>) {
        let (close, closed) = oneshot::channel();
        let console = match self {
            SerialConsole::Stream(inner) => SerialConsole::Stream(Box::new(ClosableStream {
                inner: Some(inner),
                closed,
            })),
            listener @ SerialConsole::Listener(_) => listener,
        };
        (console, close)
    }
}

/// A stream that drops its inner stream once `closed` resolves; reads then
/// return EOF and writes fail with `BrokenPipe`.
///
/// Dropping the sender wakes `poll_read`, which drops the inner stream, so a
/// reader blocked on a silent device releases it even while a
/// [`tokio::io::split`] write half is still alive.
struct ClosableStream {
    inner: Option<BoxedAsyncStream>,
    closed: oneshot::Receiver<()>,
}

impl AsyncRead for ClosableStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.inner.is_some() && Pin::new(&mut this.closed).poll(cx).is_ready() {
            this.inner = None;
        }
        match &mut this.inner {
            Some(inner) => Pin::new(inner).poll_read(cx, buf),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for ClosableStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().inner {
            Some(inner) => Pin::new(inner).poll_write(cx, buf),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().inner {
            Some(inner) => Pin::new(inner).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().inner {
            Some(inner) => Pin::new(inner).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

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
pub fn drain_to_stdio(serial: Option<SerialConsole>, channels: Vec<(LogChannel, BoxedAsyncRead)>) {
    if let Some(console) = serial {
        tokio::spawn(async move {
            match console.connect().await {
                Ok(mut stream) => {
                    let mut sink = tokio::io::stdout();
                    if let Err(e) = tokio::io::copy(&mut stream, &mut sink).await {
                        tracing::warn!(error = ?e, "serial capture drain ended with error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to connect the serial console");
                }
            }
        });
    }
    for (channel, mut reader) in channels {
        tokio::spawn(async move {
            let mut sink = tokio::io::stdout();
            if let Err(e) = tokio::io::copy(&mut reader, &mut sink).await {
                tracing::warn!(%channel, error = ?e, "capture drain ended with error");
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

    /// Dropping the close handle wakes a reader blocked on a console that
    /// never reaches EOF and closes the device, while a split write half is
    /// still alive.
    #[tokio::test]
    async fn dropping_the_close_handle_releases_the_device() {
        let (ours, mut device) = tokio::io::duplex(64);
        let (console, close) = SerialConsole::Stream(Box::new(ours)).close_on_drop();
        let stream = console.connect().await.expect("connect");
        let (mut read_half, mut write_half) = tokio::io::split(stream);

        let reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            read_half.read_to_end(&mut buf).await.map(|_| buf)
        });
        device.write_all(b"boot").await.expect("device write");
        tokio::task::yield_now().await;
        drop(close);

        let read = tokio::time::timeout(Duration::from_secs(5), reader)
            .await
            .expect("the reader was not woken by the close")
            .unwrap()
            .expect("read");
        assert_eq!(read, b"boot");

        // The device end is closed, and `write_half` now fails.
        let mut rest = Vec::new();
        device.read_to_end(&mut rest).await.expect("device read");
        assert!(rest.is_empty());
        let err = write_half.write_all(b"x").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
