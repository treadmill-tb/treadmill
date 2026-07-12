//! Durable per-channel log publisher (see `doc/log-streaming-plan.md` §2/§8).
//!
//! Each captured console channel (qemu stdout/stderr, serial) is shipped to a
//! per-job JetStream stream with a **must-not-lose** guarantee: the supervisor
//! drains the channel *fast* into a persistent per-job **spill file**, and a
//! separate shipper publishes from that file to NATS, advancing a persisted
//! **cursor only after the publish is durably acked**. On startup or after NATS
//! downtime the shipper resumes from the cursor, so no bytes are lost even
//! across a supervisor restart. The spill file is **retained** after the job
//! ends for on-host post-mortem (GC is a later, separate effort).
//!
//! The serial channel is bidirectional: when the dispatch carries a
//! console-input subject, user-typed bytes published there are delivered to
//! the guest's serial console (see [`ConsoleInput`]) — best-effort, unlike the
//! must-not-lose capture direction.
//!
//! ## Layout
//!
//! For a job whose spill directory is `<dir>`, each channel `<ch>` (the
//! [`LogChannel::as_subject_token`] value) uses two files:
//!
//! - `<dir>/<ch>.spill` — append-only frames, each `ts_ns: u64-le ++ len:
//!   u32-le ++ payload[len]`. `ts_ns` is the supervisor's wall-clock capture
//!   time; `payload` is the raw byte chunk (up to [`CHUNK_BYTES`]).
//! - `<dir>/<ch>.cursor` — 16 bytes: `next_seq: u64-le ++ byte_offset: u64-le`,
//!   the position up to which frames have been durably acked.
//!
//! ## Testability
//!
//! The spill/cursor/ship state machine is decoupled from the transport behind
//! the [`ChunkSink`] trait, so spill/ack/resume and subject/header assembly are
//! unit-tested against a stub sink with no NATS daemon. The live
//! capture→publish→read round-trip is a hermetic Nix check (the sandbox cannot
//! run `nats-server`).

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use treadmill_rs::api::switchboard_supervisor::{LogChannel, LogStreamingDispatch};

use crate::capture::SerialSocket;
use crate::launcher::BoxedAsyncRead;

/// Max bytes drained from a channel into a single spill frame (plan §2: "flush
/// on ~16 KB"). Each read of available bytes becomes one frame.
const CHUNK_BYTES: usize = 16 * 1024;

/// Spill-frame header: `ts_ns: u64-le` + `payload_len: u32-le`.
const FRAME_HEADER_LEN: u64 = 12;

/// Delay before retrying the shipper after a publish failure (NATS down).
const SHIP_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The NATS header carrying the supervisor's wall-clock capture timestamp (ns
/// since the Unix epoch), distinct from JetStream's server-side ingest time.
const TML_TS_HEADER: &str = "Tml-Ts";

/// Supervisor wall-clock capture time, ns since the Unix epoch.
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The full NATS subject a channel publishes to: `<subject_prefix>.<channel>`.
fn subject_for(subject_prefix: &str, channel: LogChannel) -> String {
    format!("{subject_prefix}.{}", channel.as_subject_token())
}

/// Per-channel monotonic message id used as the JetStream `Nats-Msg-Id` for
/// idempotent resume: `<channel>-<seq>`.
fn message_id(channel: LogChannel, seq: u64) -> String {
    format!("{}-{}", channel.as_subject_token(), seq)
}

/// The transport a channel's frames are shipped over. Behind a trait so the
/// spill/ack/resume logic is unit-testable against a stub with no NATS daemon.
#[async_trait]
pub trait ChunkSink: Send + Sync {
    /// Publish one frame and wait for the broker's **durable ack**. `msg_id` is
    /// the dedup key (`Nats-Msg-Id`); `capture_ts_ns` is the supervisor capture
    /// time. Returning `Ok` means the frame is persisted — only then does the
    /// caller advance the cursor.
    async fn publish(
        &self,
        channel: LogChannel,
        msg_id: &str,
        capture_ts_ns: u64,
        payload: &[u8],
    ) -> Result<()>;
}

/// Production [`ChunkSink`]: publishes to a per-job JetStream stream.
pub struct NatsChunkSink {
    jetstream: async_nats::jetstream::Context,
    subject_prefix: String,
}

impl NatsChunkSink {
    pub fn new(jetstream: async_nats::jetstream::Context, subject_prefix: String) -> Self {
        NatsChunkSink {
            jetstream,
            subject_prefix,
        }
    }
}

#[async_trait]
impl ChunkSink for NatsChunkSink {
    async fn publish(
        &self,
        channel: LogChannel,
        msg_id: &str,
        capture_ts_ns: u64,
        payload: &[u8],
    ) -> Result<()> {
        let subject = subject_for(&self.subject_prefix, channel);

        let mut headers = async_nats::HeaderMap::new();
        // `Nats-Msg-Id` is JetStream's dedup key (idempotent resume within the
        // stream's duplicate window); `Tml-Ts` is the supervisor capture time.
        headers.insert(async_nats::header::NATS_MESSAGE_ID, msg_id);
        headers.insert(TML_TS_HEADER, capture_ts_ns.to_string());

        let ack_future = self
            .jetstream
            .publish_with_headers(subject, headers, Bytes::copy_from_slice(payload))
            .await
            .context("publishing log frame to JetStream")?;
        // Awaiting the returned future waits for the durable publish ack.
        ack_future.await.context("awaiting JetStream publish ack")?;
        Ok(())
    }
}

/// One decoded spill frame.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    ts_ns: u64,
    payload: Vec<u8>,
}

impl Frame {
    /// On-disk size of this frame: header + payload.
    fn encoded_len(&self) -> u64 {
        FRAME_HEADER_LEN + self.payload.len() as u64
    }
}

/// Append-only writer for a channel's spill file.
struct SpillWriter {
    file: tokio::fs::File,
}

impl SpillWriter {
    async fn open(path: &Path) -> std::io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(SpillWriter { file })
    }

    /// Append a frame and flush it to the OS. We deliberately do **not** fsync:
    /// flushing hands the bytes to the kernel (so they survive a supervisor
    /// process restart — the stated durability requirement) without paying an
    /// fsync per chunk, which would slow the drain and risk losing bytes at the
    /// (backpressure-free) source. A kernel panic / power loss may drop the
    /// not-yet-written-back tail.
    async fn append(&mut self, ts_ns: u64, payload: &[u8]) -> std::io::Result<()> {
        let len = payload.len() as u32;
        self.file.write_all(&ts_ns.to_le_bytes()).await?;
        self.file.write_all(&len.to_le_bytes()).await?;
        self.file.write_all(payload).await?;
        self.file.flush().await?;
        Ok(())
    }
}

/// Read the next frame from `reader`, or `None` at a clean end-of-file (or a
/// torn tail from a crash mid-append — treated as end, never as a partial
/// publish).
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; FRAME_HEADER_LEN as usize];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let ts_ns = u64::from_le_bytes(header[0..8].try_into().unwrap());
    let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;

    let mut payload = vec![0u8; len];
    match reader.read_exact(&mut payload).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    Ok(Some(Frame { ts_ns, payload }))
}

/// Persisted ship cursor: the next sequence number and byte offset to publish.
struct Cursor {
    path: PathBuf,
    next_seq: u64,
    offset: u64,
}

impl Cursor {
    /// Load the cursor, or start at `(0, 0)` if absent (or unreadably short —
    /// re-shipping from the start is safe, JetStream dedups by `Nats-Msg-Id`).
    async fn load(path: &Path) -> std::io::Result<Self> {
        match tokio::fs::read(path).await {
            Ok(bytes) if bytes.len() == 16 => Ok(Cursor {
                path: path.to_path_buf(),
                next_seq: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
                offset: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            }),
            Ok(_) => Ok(Cursor {
                path: path.to_path_buf(),
                next_seq: 0,
                offset: 0,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cursor {
                path: path.to_path_buf(),
                next_seq: 0,
                offset: 0,
            }),
            Err(e) => Err(e),
        }
    }

    /// Persist the cursor atomically (write a temp file, then rename).
    async fn store(&self) -> std::io::Result<()> {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&self.next_seq.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        tokio::fs::write(&tmp, buf).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

/// Publish every frame from the cursor's offset to the current end of the spill
/// file, advancing (and persisting) the cursor **after each durable ack**.
///
/// On a publish failure this returns `Err` with the cursor left at the last
/// acked frame, so a retry resumes from exactly there — no loss, and the
/// `Nats-Msg-Id` dedup absorbs any frame the broker persisted but didn't ack in
/// time.
async fn ship_pending(
    spill_path: &Path,
    cursor: &mut Cursor,
    sink: &dyn ChunkSink,
    channel: LogChannel,
) -> Result<()> {
    let file = match tokio::fs::File::open(spill_path).await {
        Ok(file) => file,
        // Nothing spilled yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("opening spill file"),
    };

    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(cursor.offset))
        .await
        .context("seeking to spill cursor")?;

    while let Some(frame) = read_frame(&mut reader)
        .await
        .context("reading spill frame")?
    {
        let msg_id = message_id(channel, cursor.next_seq);
        sink.publish(channel, &msg_id, frame.ts_ns, &frame.payload)
            .await
            .with_context(|| format!("publishing frame {msg_id}"))?;

        cursor.next_seq += 1;
        cursor.offset += frame.encoded_len();
        cursor.store().await.context("persisting spill cursor")?;
    }
    Ok(())
}

/// Drain `reader` into the spill file and ship spilled frames to `sink`,
/// resuming from the persisted cursor. Returns when the reader hits EOF and all
/// frames are durably shipped; the spill file is retained.
async fn run_channel(
    inner: Arc<Inner>,
    channel: LogChannel,
    mut reader: BoxedAsyncRead,
) -> Result<()> {
    tokio::fs::create_dir_all(&inner.spill_dir)
        .await
        .context("creating spill directory")?;
    let token = channel.as_subject_token();
    let spill_path = inner.spill_dir.join(format!("{token}.spill"));
    let cursor_path = inner.spill_dir.join(format!("{token}.cursor"));

    let mut writer = SpillWriter::open(&spill_path)
        .await
        .context("opening spill file for append")?;

    let notify = Arc::new(Notify::new());
    let eof = Arc::new(AtomicBool::new(false));

    // Drain task: reader → spill file, as fast as the disk allows. Decoupled
    // from the shipper so a slow/disconnected NATS never backpressures the
    // (backpressure-free) capture source.
    let drain = {
        let notify = notify.clone();
        let eof = eof.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK_BYTES];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = writer.append(now_ns(), &buf[..n]).await {
                            tracing::error!(channel = token, error = ?e, "failed to append to spill file");
                            break;
                        }
                        notify.notify_one();
                    }
                    Err(e) => {
                        tracing::warn!(channel = token, error = ?e, "capture read error; ending drain");
                        break;
                    }
                }
            }
            eof.store(true, Ordering::SeqCst);
            notify.notify_one();
        })
    };

    // Ship loop: spill file → NATS, advancing the cursor on ack. Retries from
    // the cursor on publish failure (must-not-lose); exits once the reader is
    // at EOF and the file is fully shipped.
    let mut cursor = Cursor::load(&cursor_path)
        .await
        .context("loading spill cursor")?;
    loop {
        // Arm before shipping so a notify during the publish is not lost.
        let armed = notify.notified();
        match ship_pending(&spill_path, &mut cursor, inner.sink.as_ref(), channel).await {
            Ok(()) => {
                if eof.load(Ordering::SeqCst) {
                    // No more frames will be written after eof is observed;
                    // ship any that landed between the last pass and eof.
                    ship_pending(&spill_path, &mut cursor, inner.sink.as_ref(), channel).await?;
                    break;
                }
                armed.await;
            }
            Err(e) => {
                tracing::warn!(channel = token, error = ?e, "shipping log frames failed; retrying from cursor");
                drop(armed);
                tokio::time::sleep(SHIP_RETRY_DELAY).await;
            }
        }
    }

    let _ = drain.await;
    Ok(())
}

/// Connect to NATS with a **bearer** write token: present the JWT, sign no
/// nonce. `async-nats` only emits a `user_jwt` without a signature via the
/// auth-callback path (the `with_jwt` builder always installs a signer), so we
/// return an [`async_nats::Auth`] carrying just the JWT. The server accepts it
/// because the token is minted with `bearer_token: true`.
async fn bearer_connect(
    nats_url: &str,
    write_token: &str,
    inbox_prefix: Option<&str>,
) -> Result<async_nats::Client> {
    let token = write_token.to_owned();
    let mut options = async_nats::ConnectOptions::with_auth_callback(move |_nonce| {
        let token = token.clone();
        async move {
            let mut auth = async_nats::Auth::new();
            auth.jwt = Some(token);
            Ok(auth)
        }
    })
    .name("tml-supervisor-logstream")
    // Don't fail (or lose capture) if the broker is briefly down at job start:
    // connect succeeds, the client reconnects in the background, and the
    // shipper retries from the cursor while the drain keeps spilling.
    .retry_on_initial_connect();
    // The write token only permits subscribing to reply inboxes under this
    // prefix; without it, JetStream publish acks would never arrive.
    if let Some(prefix) = inbox_prefix {
        options = options.custom_inbox_prefix(prefix);
    }
    options
        .connect(nats_url)
        .await
        .with_context(|| format!("connecting to NATS at {nats_url}"))
}

/// The console-input side channel: a core-NATS subscription on the job's
/// input subject, whose message payloads are written to the guest's serial
/// console. Server-side recording happens in parallel at the broker (the
/// input stream is bound to the subject), so the supervisor just delivers.
struct ConsoleInput {
    client: async_nats::Client,
    subject: String,
}

/// Subscribe to the console-input subject and write each message's payload to
/// `writer` (the serial connection's write half), flushing per message.
///
/// Ends on subscribe/write error (logged) or when the subscription closes;
/// qemu exiting closes the socket, which surfaces as a write error on the next
/// message and terminates the task naturally. Input published while no
/// subscription is live is dropped, never replayed stale.
async fn run_console_input(input: ConsoleInput, mut writer: impl tokio::io::AsyncWrite + Unpin) {
    use futures_util::StreamExt as _;

    let mut subscription = match input.client.subscribe(input.subject.clone()).await {
        Ok(subscription) => subscription,
        Err(e) => {
            tracing::warn!(
                subject = input.subject,
                error = ?e,
                "failed to subscribe to console input; typed input will not be delivered",
            );
            return;
        }
    };

    while let Some(message) = subscription.next().await {
        let write = async {
            writer.write_all(&message.payload).await?;
            writer.flush().await
        };
        if let Err(e) = write.await {
            tracing::warn!(
                subject = input.subject,
                error = ?e,
                "writing console input to the serial console failed; ending input delivery",
            );
            return;
        }
    }
}

struct Inner {
    sink: Arc<dyn ChunkSink>,
    spill_dir: PathBuf,
    console_input: Option<ConsoleInput>,
}

/// Spawns the durable capture publisher for a job's log channels.
///
/// Cheaply cloneable (an `Arc` inside) so it can move into the serial-accept
/// task. Built either from a [`LogStreamingDispatch`] (production, connects to
/// NATS with the bearer write token) or directly from a [`ChunkSink`] (tests /
/// the hermetic check).
#[derive(Clone)]
pub struct LogPublisher {
    inner: Arc<Inner>,
}

impl LogPublisher {
    /// Connect to NATS using the dispatch's bearer write token and build a
    /// publisher whose spill files live under `spill_dir`. When the dispatch
    /// carries a console-input subject, the serial channel also delivers
    /// typed input from that subject to the guest (see
    /// [`spawn_serial`](Self::spawn_serial)).
    pub async fn connect(
        dispatch: &LogStreamingDispatch,
        spill_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        let client = bearer_connect(
            &dispatch.nats_url,
            &dispatch.write_token,
            dispatch.inbox_prefix.as_deref(),
        )
        .await?;
        let console_input = dispatch
            .console_input_subject
            .clone()
            .map(|subject| ConsoleInput {
                client: client.clone(),
                subject,
            });
        let jetstream = async_nats::jetstream::new(client);
        let sink: Arc<dyn ChunkSink> = Arc::new(NatsChunkSink::new(
            jetstream,
            dispatch.subject_prefix.clone(),
        ));
        Ok(LogPublisher {
            inner: Arc::new(Inner {
                sink,
                spill_dir: spill_dir.into(),
                console_input,
            }),
        })
    }

    /// Build a publisher over an arbitrary [`ChunkSink`] (composition seam for
    /// tests and the hermetic NATS check). No console input is delivered.
    pub fn with_sink(sink: Arc<dyn ChunkSink>, spill_dir: impl Into<PathBuf>) -> Self {
        LogPublisher {
            inner: Arc::new(Inner {
                sink,
                spill_dir: spill_dir.into(),
                console_input: None,
            }),
        }
    }

    /// Spawn the drain+ship tasks for a channel backed by `reader` (qemu
    /// stdout/stderr).
    pub fn spawn_channel(&self, channel: LogChannel, reader: BoxedAsyncRead) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            if let Err(e) = run_channel(inner, channel, reader).await {
                tracing::error!(
                    channel = channel.as_subject_token(),
                    error = ?e,
                    "log channel publisher exited with error",
                );
            }
        });
    }

    /// Spawn the publisher for the serial channel: accept qemu's serial-socket
    /// connection off the hot path, then drain+ship it. With console input
    /// configured, the connection is split and a parallel task delivers typed
    /// input from the input subject to the write half.
    pub fn spawn_serial(&self, channel: LogChannel, socket: SerialSocket) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            match socket.accept().await {
                Ok(stream) => {
                    let reader: BoxedAsyncRead = match &inner.console_input {
                        Some(input) => {
                            let (read_half, write_half) = stream.into_split();
                            let input = ConsoleInput {
                                client: input.client.clone(),
                                subject: input.subject.clone(),
                            };
                            tokio::spawn(run_console_input(input, write_half));
                            Box::new(read_half)
                        }
                        None => Box::new(stream),
                    };
                    if let Err(e) = run_channel(inner, channel, reader).await {
                        tracing::error!(error = ?e, "serial channel publisher exited with error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to accept qemu serial connection for log streaming");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A captured publish: the args `ship_pending` handed the sink.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Published {
        channel: LogChannel,
        msg_id: String,
        ts_ns: u64,
        payload: Vec<u8>,
    }

    /// Stub [`ChunkSink`] recording every publish; optionally fails once the
    /// recorded count reaches `fail_after` (to drive the resume path).
    struct RecordingSink {
        published: Mutex<Vec<Published>>,
        fail_after: Option<usize>,
    }

    impl RecordingSink {
        fn new() -> Self {
            RecordingSink {
                published: Mutex::new(Vec::new()),
                fail_after: None,
            }
        }
        fn failing_after(n: usize) -> Self {
            RecordingSink {
                published: Mutex::new(Vec::new()),
                fail_after: Some(n),
            }
        }
        fn records(&self) -> Vec<Published> {
            self.published.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChunkSink for RecordingSink {
        async fn publish(
            &self,
            channel: LogChannel,
            msg_id: &str,
            capture_ts_ns: u64,
            payload: &[u8],
        ) -> Result<()> {
            {
                let recorded = self.published.lock().unwrap();
                if Some(recorded.len()) == self.fail_after {
                    anyhow::bail!("simulated publish failure");
                }
            }
            self.published.lock().unwrap().push(Published {
                channel,
                msg_id: msg_id.to_string(),
                ts_ns: capture_ts_ns,
                payload: payload.to_vec(),
            });
            Ok(())
        }
    }

    async fn write_spill(path: &Path, frames: &[(u64, &[u8])]) {
        let mut w = SpillWriter::open(path).await.unwrap();
        for (ts, payload) in frames {
            w.append(*ts, payload).await.unwrap();
        }
    }

    #[test]
    fn subject_and_message_id_assembly() {
        assert_eq!(
            subject_for("logs.abc-123", LogChannel::QemuStdout),
            "logs.abc-123.qemu-stdout"
        );
        assert_eq!(
            subject_for("logs.abc-123", LogChannel::Serial),
            "logs.abc-123.serial"
        );
        assert_eq!(message_id(LogChannel::QemuStderr, 7), "qemu-stderr-7");
    }

    #[tokio::test]
    async fn spill_file_round_trips_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qemu-stdout.spill");
        write_spill(&path, &[(1, b"alpha"), (2, b""), (3, b"gamma")]).await;

        let file = tokio::fs::File::open(&path).await.unwrap();
        let mut reader = BufReader::new(file);
        let mut got = Vec::new();
        while let Some(frame) = read_frame(&mut reader).await.unwrap() {
            got.push(frame);
        }
        assert_eq!(
            got,
            vec![
                Frame {
                    ts_ns: 1,
                    payload: b"alpha".to_vec()
                },
                Frame {
                    ts_ns: 2,
                    payload: b"".to_vec()
                },
                Frame {
                    ts_ns: 3,
                    payload: b"gamma".to_vec()
                },
            ]
        );
    }

    #[tokio::test]
    async fn cursor_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qemu-stdout.cursor");

        // Absent → starts at zero.
        let fresh = Cursor::load(&path).await.unwrap();
        assert_eq!((fresh.next_seq, fresh.offset), (0, 0));

        let saved = Cursor {
            path: path.clone(),
            next_seq: 4,
            offset: 4096,
        };
        saved.store().await.unwrap();

        let reloaded = Cursor::load(&path).await.unwrap();
        assert_eq!((reloaded.next_seq, reloaded.offset), (4, 4096));
    }

    #[tokio::test]
    async fn ships_all_frames_in_order_and_advances_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("qemu-stdout.spill");
        let cursor_path = dir.path().join("qemu-stdout.cursor");
        write_spill(&spill, &[(10, b"one"), (20, b"two"), (30, b"three")]).await;

        let sink = RecordingSink::new();
        let mut cursor = Cursor::load(&cursor_path).await.unwrap();
        ship_pending(&spill, &mut cursor, &sink, LogChannel::QemuStdout)
            .await
            .unwrap();

        let records = sink.records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].msg_id, "qemu-stdout-0");
        assert_eq!(records[0].payload, b"one");
        assert_eq!(records[0].ts_ns, 10);
        assert_eq!(records[1].msg_id, "qemu-stdout-1");
        assert_eq!(records[2].msg_id, "qemu-stdout-2");
        assert_eq!(records[2].payload, b"three");

        // Cursor is at end-of-file; a second pass ships nothing.
        let file_len = tokio::fs::metadata(&spill).await.unwrap().len();
        assert_eq!(cursor.offset, file_len);
        assert_eq!(cursor.next_seq, 3);

        ship_pending(&spill, &mut cursor, &sink, LogChannel::QemuStdout)
            .await
            .unwrap();
        assert_eq!(sink.records().len(), 3, "no re-publish at EOF");
    }

    /// A publish failure mid-stream leaves the cursor at the last ack; resuming
    /// re-ships exactly the un-acked frames (no loss, no re-publish of acked
    /// frames). This is the must-not-lose / resume guarantee.
    #[tokio::test]
    async fn resume_after_failure_reships_only_unacked_frames() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("serial.spill");
        let cursor_path = dir.path().join("serial.cursor");
        write_spill(
            &spill,
            &[(1, b"f0"), (2, b"f1"), (3, b"f2"), (4, b"f3"), (5, b"f4")],
        )
        .await;

        // Fail on the 3rd publish (index 2): frames 0 and 1 ack, 2 fails.
        let failing = RecordingSink::failing_after(2);
        let mut cursor = Cursor::load(&cursor_path).await.unwrap();
        let err = ship_pending(&spill, &mut cursor, &failing, LogChannel::Serial).await;
        assert!(err.is_err());
        assert_eq!(cursor.next_seq, 2, "cursor advanced only past acked frames");
        let acked = failing.records();
        assert_eq!(acked.len(), 2);
        assert_eq!(acked[0].msg_id, "serial-0");
        assert_eq!(acked[1].msg_id, "serial-1");

        // The cursor was persisted; reload it (as a restart would) and resume
        // against a healthy sink.
        let mut resumed = Cursor::load(&cursor_path).await.unwrap();
        assert_eq!(resumed.next_seq, 2);
        let healthy = RecordingSink::new();
        ship_pending(&spill, &mut resumed, &healthy, LogChannel::Serial)
            .await
            .unwrap();

        let reshipped: Vec<_> = healthy.records().iter().map(|p| p.msg_id.clone()).collect();
        assert_eq!(reshipped, vec!["serial-2", "serial-3", "serial-4"]);
        assert_eq!(
            healthy.records()[0].payload,
            b"f2",
            "resumes at the first un-acked frame",
        );
    }

    // ---- Live NATS round-trip (hermetic Nix check) -----------------------
    //
    // Gated on `TML_TEST_NATS_SERVER` (the nats-server binary), set only by the
    // `nats-log-streaming` Nix check. Unset (plain `nextest` / the sandbox) →
    // the test skips, since `nats-server` binds a TCP port and can't run in the
    // restricted sandbox (AGENTS.md §2).

    fn nats_server_bin() -> Option<PathBuf> {
        std::env::var_os("TML_TEST_NATS_SERVER")
            .map(PathBuf::from)
            .filter(|p| p.is_file())
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A child `nats-server` with JetStream enabled (no auth); killed on drop.
    struct NatsServer {
        child: std::process::Child,
        port: u16,
        _dir: tempfile::TempDir,
    }

    impl Drop for NatsServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl NatsServer {
        fn url(&self) -> String {
            format!("nats://127.0.0.1:{}", self.port)
        }

        async fn start(bin: &Path) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = dir.path().join("js");
            let port = free_port();
            let child = std::process::Command::new(bin)
                .arg("-js")
                .arg("-sd")
                .arg(&store)
                .arg("-a")
                .arg("127.0.0.1")
                .arg("-p")
                .arg(port.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn nats-server");
            let server = NatsServer {
                child,
                port,
                _dir: dir,
            };
            for _ in 0..100 {
                if async_nats::connect(server.url()).await.is_ok() {
                    return server;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("nats-server did not become ready");
        }
    }

    #[tokio::test]
    async fn nats_live_publish_and_read() {
        let Some(bin) = nats_server_bin() else {
            eprintln!("TML_TEST_NATS_SERVER unset; skipping live NATS publish test");
            return;
        };
        let server = NatsServer::start(&bin).await;
        let client = async_nats::connect(server.url()).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);

        // The switchboard would have created this stream at dispatch.
        let job = "testjob";
        jetstream
            .get_or_create_stream(async_nats::jetstream::stream::Config {
                name: format!("logs-{job}"),
                subjects: vec![format!("logs.{job}.>")],
                max_age: Duration::ZERO,
                ..Default::default()
            })
            .await
            .unwrap();

        // Publish three frames through the real ship path + NatsChunkSink.
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("qemu-stdout.spill");
        let cursor_path = dir.path().join("qemu-stdout.cursor");
        write_spill(
            &spill,
            &[(111, b"hello "), (222, b"there\n"), (333, b"world")],
        )
        .await;

        let sink = NatsChunkSink::new(jetstream.clone(), format!("logs.{job}"));
        let mut cursor = Cursor::load(&cursor_path).await.unwrap();
        ship_pending(&spill, &mut cursor, &sink, LogChannel::QemuStdout)
            .await
            .unwrap();

        // Read the stream back: subject, headers, payloads, in order.
        let stream = jetstream.get_stream(format!("logs-{job}")).await.unwrap();
        let consumer = stream
            .get_or_create_consumer(
                "test-reader",
                async_nats::jetstream::consumer::pull::Config {
                    name: Some("test-reader".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut batch = consumer.fetch().max_messages(3).messages().await.unwrap();
        let mut got = Vec::new();
        while let Some(message) = futures_util::StreamExt::next(&mut batch).await {
            let message = message.unwrap();
            let headers = message.headers.clone().expect("frame has headers");
            let msg_id = headers
                .get(async_nats::header::NATS_MESSAGE_ID)
                .unwrap()
                .as_str()
                .to_string();
            let ts = headers.get(TML_TS_HEADER).unwrap().as_str().to_string();
            got.push((
                message.subject.to_string(),
                msg_id,
                ts,
                message.payload.to_vec(),
            ));
            let _ = message.ack().await;
        }

        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            (
                "logs.testjob.qemu-stdout".into(),
                "qemu-stdout-0".into(),
                "111".into(),
                b"hello ".to_vec()
            )
        );
        assert_eq!(
            got[1],
            (
                "logs.testjob.qemu-stdout".into(),
                "qemu-stdout-1".into(),
                "222".into(),
                b"there\n".to_vec()
            )
        );
        assert_eq!(
            got[2],
            (
                "logs.testjob.qemu-stdout".into(),
                "qemu-stdout-2".into(),
                "333".into(),
                b"world".to_vec()
            )
        );
    }

    /// Bytes published to the console-input subject reach the serial peer (a
    /// fake qemu on the other end of the socket) and — with the recording
    /// stream bound to the subject, as the switchboard provisions before any
    /// input token exists — are captured server-side in `console-in-<job>`.
    #[tokio::test]
    async fn nats_live_console_input_reaches_the_serial_peer_and_is_recorded() {
        use tokio::io::AsyncReadExt as _;

        let Some(bin) = nats_server_bin() else {
            eprintln!("TML_TEST_NATS_SERVER unset; skipping live NATS console-input test");
            return;
        };
        let server = NatsServer::start(&bin).await;
        let client = async_nats::connect(server.url()).await.unwrap();
        let jetstream = async_nats::jetstream::new(client.clone());

        let job = "testjob";
        let subject = format!("console-in.{job}");
        jetstream
            .get_or_create_stream(async_nats::jetstream::stream::Config {
                name: format!("console-in-{job}"),
                subjects: vec![subject.clone()],
                max_age: Duration::ZERO,
                ..Default::default()
            })
            .await
            .unwrap();

        // Bind the serial socket and connect the fake-qemu peer (the listener
        // backlog holds the connection until `spawn_serial` accepts it).
        let dir = tempfile::tempdir().unwrap();
        let socket = crate::capture::SerialSocket::bind(dir.path().join("serial.sock"))
            .await
            .unwrap();
        let mut peer = tokio::net::UnixStream::connect(socket.path())
            .await
            .unwrap();

        let publisher = LogPublisher {
            inner: Arc::new(Inner {
                sink: Arc::new(RecordingSink::new()),
                spill_dir: dir.path().join("logs"),
                console_input: Some(ConsoleInput {
                    client: client.clone(),
                    subject: subject.clone(),
                }),
            }),
        };
        publisher.spawn_serial(LogChannel::Serial, socket);

        // The input task subscribes asynchronously and a core publish with no
        // live subscription is dropped (by design), so republish until the
        // peer sees input.
        let payload = b"input!";
        let mut buf = [0u8; 64];
        let mut n = 0;
        for attempt in 0.. {
            assert!(attempt < 100, "peer never received console input");
            client
                .publish(subject.clone(), Bytes::from_static(payload))
                .await
                .unwrap();
            client.flush().await.unwrap();
            match tokio::time::timeout(Duration::from_millis(300), peer.read(&mut buf)).await {
                Ok(read) => {
                    n = read.expect("peer read");
                    break;
                }
                Err(_elapsed) => continue,
            }
        }
        // One `read` may return several coalesced messages; every one is a
        // copy of the payload, so checking the head suffices.
        assert!(n >= payload.len(), "read at least one full payload");
        assert_eq!(&buf[..payload.len()], payload);

        // Every accepted publish was recorded server-side by the
        // subject-bound stream, no JetStream involvement from the client.
        let mut stream = jetstream
            .get_stream(format!("console-in-{job}"))
            .await
            .unwrap();
        let state = &stream.info().await.unwrap().state;
        assert!(state.messages >= 1, "typed input must be recorded");
        let first = stream.get_raw_message(1).await.unwrap();
        assert_eq!(first.subject.as_str(), subject);
        assert_eq!(first.payload.as_ref(), payload);
    }
}
