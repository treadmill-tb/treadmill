//! Forwarding the supervisor's own tracing events into a job's log stream.
//!
//! A [`tracing_subscriber::Layer`] sits beside the terminal `fmt` layer and
//! routes each event to the job whose span it was emitted in. **Only events
//! emitted inside a `job_id` span are forwarded**, which is the security
//! boundary: connector authentication, config parsing and process-level
//! credentials are emitted outside any job span and cannot reach a job's
//! stream however they are formatted.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::StreamReader;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;

use crate::launcher::BoxedAsyncRead;

/// How many serialized events a job's channel buffers before the layer starts
/// dropping them. Sized to cover the window between a job's registration and
/// its publisher being attached.
const JOB_LOG_CAPACITY: usize = 256;

/// The live per-job senders the [`Layer`] routes events to.
///
/// Keyed by the *formatted* job id: both `?value` and `%value` reach a field
/// visitor through `record_debug`, and `Uuid`'s `Debug` and `Display` agree,
/// so this works whichever sigil an `#[instrument]` attribute uses and costs
/// no parse per event.
#[derive(Clone, Default)]
pub struct JobLogRegistry {
    jobs: Arc<Mutex<HashMap<String, mpsc::Sender<Bytes>>>>,
}

impl fmt::Debug for JobLogRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobLogRegistry").finish_non_exhaustive()
    }
}

impl JobLogRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start forwarding this job's events. Events are buffered until the
    /// reader is attached to a publisher, and dropped once the buffer fills —
    /// which is what happens for a job dispatched without log streaming.
    pub fn register(&self, job_id: Uuid) -> JobLogRegistration {
        let (tx, rx) = mpsc::channel(JOB_LOG_CAPACITY);
        let key = job_id.to_string();
        self.jobs.lock().unwrap().insert(key.clone(), tx);
        JobLogRegistration {
            registry: self.clone(),
            job_id: key,
            reader: Some(rx),
        }
    }

    fn sender(&self, job_id: &str) -> Option<mpsc::Sender<Bytes>> {
        self.jobs.lock().unwrap().get(job_id).cloned()
    }
}

/// A job's registration in the [`JobLogRegistry`].
///
/// Dropping it unregisters the job and closes the channel, so an attached
/// reader sees EOF and the publisher can finish draining.
pub struct JobLogRegistration {
    registry: JobLogRegistry,
    job_id: String,
    reader: Option<mpsc::Receiver<Bytes>>,
}

impl JobLogRegistration {
    /// The reading end, once. `None` on a second call.
    pub fn take_reader(&mut self) -> Option<BoxedAsyncRead> {
        self.reader.take().map(channel_reader)
    }
}

impl Drop for JobLogRegistration {
    fn drop(&mut self) {
        self.registry.jobs.lock().unwrap().remove(&self.job_id);
    }
}

/// Adapt a receiver of framed bytes to the `AsyncRead` the publisher consumes,
/// so a produced channel rides the same spill/ack/resume path as a captured one.
pub fn channel_reader(rx: mpsc::Receiver<Bytes>) -> BoxedAsyncRead {
    Box::new(StreamReader::new(
        ReceiverStream::new(rx).map(Ok::<_, io::Error>),
    ))
}

/// Install the process's tracing subscriber: the terminal `fmt` layer, plus
/// the layer forwarding job-scoped events into the returned registry.
///
/// The two are filtered independently — `RUST_LOG` (or, unset, INFO) governs
/// the terminal, `job_log_level` governs what a job's readers see — so
/// operator verbosity and user-visible verbosity are decoupled.
pub fn init_tracing(job_log_level: &str) -> Result<JobLogRegistry> {
    let job_log_level: LevelFilter = job_log_level
        .parse()
        .with_context(|| format!("Parsing job log level {job_log_level:?}"))?;

    let registry = JobLogRegistry::new();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::default().add_directive(LevelFilter::INFO.into())
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter))
        .with(JobLogLayer::new(registry.clone(), job_log_level))
        .init();

    Ok(registry)
}

/// The job id a span recorded, stashed in its extensions by
/// [`JobLogLayer::on_new_span`].
#[derive(Clone)]
struct SpanJobId(String);

struct JobLogLayer {
    registry: JobLogRegistry,
    level: LevelFilter,
}

impl JobLogLayer {
    fn new(registry: JobLogRegistry, level: LevelFilter) -> Self {
        JobLogLayer { registry, level }
    }
}

impl<S> Layer<S> for JobLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(self.level)
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = JobIdVisitor(None);
        attrs.record(&mut visitor);
        if let Some(job_id) = visitor.0
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(SpanJobId(job_id));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if LevelFilter::from_level(*event.metadata().level()) > self.level {
            return;
        }

        let Some(job_id) = ctx.event_scope(event).and_then(|mut scope| {
            scope.find_map(|span| span.extensions().get::<SpanJobId>().cloned())
        }) else {
            return;
        };

        let Some(sender) = self.registry.sender(&job_id.0) else {
            return;
        };

        // A full channel drops the event. Reporting that here would emit an
        // event from inside the handler for one.
        let _ = sender.try_send(event_line(event));
    }
}

/// Picks the `job_id` field out of a span's fields.
struct JobIdVisitor(Option<String>);

impl Visit for JobIdVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "job_id" {
            self.0 = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "job_id" {
            self.0 = Some(value.to_string());
        }
    }
}

/// One event as a single JSONL line.
///
/// The event's own fields nest under `fields` so a user-supplied field name
/// cannot collide with the envelope's keys.
fn event_line(event: &Event<'_>) -> Bytes {
    let mut visitor = EventVisitor {
        message: String::new(),
        fields: serde_json::Map::new(),
    };
    event.record(&mut visitor);

    let metadata = event.metadata();
    let mut line = serde_json::to_vec(&serde_json::json!({
        "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "level": metadata.level().as_str(),
        "target": metadata.target(),
        "message": visitor.message,
        "fields": visitor.fields,
    }))
    .unwrap_or_default();
    line.push(b'\n');
    Bytes::from(line)
}

struct EventVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl EventVisitor {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::String(value));
        }
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tracing::{Level, event, info_span};
    use tracing_subscriber::layer::SubscriberExt;

    /// Drive `body` under a subscriber carrying only the job-log layer.
    fn with_layer(registry: &JobLogRegistry, level: LevelFilter, body: impl FnOnce()) {
        let subscriber =
            tracing_subscriber::registry().with(JobLogLayer::new(registry.clone(), level));
        tracing::subscriber::with_default(subscriber, body);
    }

    fn drain(rx: &mut mpsc::Receiver<Bytes>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(line) = rx.try_recv() {
            out.push(serde_json::from_slice(&line).expect("each line is one JSON document"));
        }
        out
    }

    #[test]
    fn routes_events_to_the_job_whose_span_they_are_in() {
        let registry = JobLogRegistry::new();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let mut registration = registry.register(mine);
        let mut other = registry.register(theirs);

        with_layer(&registry, LevelFilter::INFO, || {
            info_span!("run", job_id = ?mine).in_scope(|| {
                event!(Level::INFO, answer = 42, "in my job");
            });
            info_span!("run", job_id = ?theirs).in_scope(|| {
                event!(Level::INFO, "in the other job");
            });
        });

        let lines = drain(registration.reader.as_mut().unwrap());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["message"], "in my job");
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["fields"]["answer"], "42");

        let other = drain(other.reader.as_mut().unwrap());
        assert_eq!(other.len(), 1);
        assert_eq!(other[0]["message"], "in the other job");
    }

    #[test]
    fn ignores_events_outside_any_job_span() {
        let registry = JobLogRegistry::new();
        let job_id = Uuid::new_v4();
        let mut registration = registry.register(job_id);

        with_layer(&registry, LevelFilter::INFO, || {
            event!(Level::INFO, "no span at all");
            info_span!("connect", url = "nats://example").in_scope(|| {
                event!(Level::INFO, "a span carrying no job id");
            });
        });

        assert!(drain(registration.reader.as_mut().unwrap()).is_empty());
    }

    #[test]
    fn a_full_channel_drops_rather_than_blocks() {
        let registry = JobLogRegistry::new();
        let job_id = Uuid::new_v4();
        let mut registration = registry.register(job_id);

        with_layer(&registry, LevelFilter::INFO, || {
            info_span!("run", job_id = ?job_id).in_scope(|| {
                for i in 0..JOB_LOG_CAPACITY * 2 {
                    event!(Level::INFO, i, "flood");
                }
            });
        });

        assert_eq!(
            drain(registration.reader.as_mut().unwrap()).len(),
            JOB_LOG_CAPACITY
        );
    }

    #[test]
    fn events_below_the_level_are_not_forwarded() {
        let registry = JobLogRegistry::new();
        let job_id = Uuid::new_v4();
        let mut registration = registry.register(job_id);

        with_layer(&registry, LevelFilter::INFO, || {
            info_span!("run", job_id = ?job_id).in_scope(|| {
                event!(Level::DEBUG, "too quiet");
                event!(Level::WARN, "loud enough");
            });
        });

        let lines = drain(registration.reader.as_mut().unwrap());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["message"], "loud enough");
    }

    /// The whole path a supervisor event travels: layer → registry → channel
    /// → publisher → sink, alongside a declaration on `meta`.
    #[tokio::test]
    async fn events_and_declarations_reach_the_publisher() {
        use crate::publisher::tests::RecordingSink;
        use crate::publisher::{LogPublisher, LogPublisherConfig};
        use treadmill_rs::api::switchboard_supervisor::{
            LOG_VIEW_MANIFEST_VERSION, LogChannel, LogFormat, LogRender, LogView, LogViewManifest,
        };

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(RecordingSink::new());
        let publisher = LogPublisher::with_sink(
            sink.clone(),
            dir.path(),
            LogPublisherConfig {
                flush_interval: std::time::Duration::from_millis(10),
                ..LogPublisherConfig::default()
            },
        );

        let registry = JobLogRegistry::new();
        let job_id = Uuid::new_v4();
        let mut registration = registry.register(job_id);
        publisher.spawn_channel(LogChannel::Supervisor, registration.take_reader().unwrap());

        let (meta_tx, meta_rx) = mpsc::channel(8);
        publisher.spawn_channel(LogChannel::Meta, channel_reader(meta_rx));

        with_layer(&registry, LevelFilter::INFO, || {
            info_span!("run", job_id = ?job_id).in_scope(|| {
                event!(Level::INFO, "fetching the image");
            });
        });

        let manifest = LogViewManifest {
            version: LOG_VIEW_MANIFEST_VERSION,
            views: vec![LogView {
                id: "supervisor".to_string(),
                label: "Supervisor".to_string(),
                render: LogRender::Text,
                format: LogFormat::Jsonl,
                channels: vec![LogChannel::Supervisor],
                order: 30,
                default: false,
                input: false,
            }],
        };
        let mut line = serde_json::to_vec(&manifest).unwrap();
        line.push(b'\n');
        meta_tx.send(Bytes::from(line)).await.unwrap();

        drop(registration);
        drop(meta_tx);
        publisher.drain(std::time::Duration::from_secs(5)).await;

        let published = sink.records();
        let supervisor: Vec<_> = published
            .iter()
            .filter(|p| p.channel == LogChannel::Supervisor)
            .collect();
        assert_eq!(supervisor.len(), 1);
        let event: serde_json::Value = serde_json::from_slice(&supervisor[0].payload).unwrap();
        assert_eq!(event["message"], "fetching the image");

        let meta: Vec<_> = published
            .iter()
            .filter(|p| p.channel == LogChannel::Meta)
            .collect();
        assert_eq!(meta.len(), 1);
        let declared: LogViewManifest = serde_json::from_slice(&meta[0].payload).unwrap();
        assert_eq!(declared, manifest);
    }

    #[test]
    fn dropping_the_registration_closes_the_channel() {
        let registry = JobLogRegistry::new();
        let job_id = Uuid::new_v4();
        let mut registration = registry.register(job_id);
        let mut rx = registration.reader.take().unwrap();

        drop(registration);
        assert!(rx.try_recv().is_err());
        assert!(registry.sender(&job_id.to_string()).is_none());
    }
}
