//! Log routing, and Sentry error reporting when `[sentry]` is configured.
//!
//! Log levels:
//! - `error!` -- the switchboard, or something it depends on, is broken.
//! - `warn!` -- a human should look eventually. May be client-triggered.
//! - `info!` -- normal operation, including requests the server correctly
//!   refuses. Reported only as breadcrumbs, for context on a real event.
//! - `debug!` and below only for diagnosing issues. Bearer tokens, client
//!   addresses, and provider usernames may only be logged here.

use anyhow::Context;
use sentry::integrations::tracing::{BreadcrumbLayer, ErrorLayer, SpanLayer};
use tracing::Level;
use tracing_subscriber::filter::{LevelFilter, filter_fn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::{SentryConfig, SentryEventLevel};

/// Install the global tracing subscriber, and the Sentry client if configured.
/// The returned guard flushes pending events when dropped.
pub fn init(config: Option<&SentryConfig>) -> anyhow::Result<Option<sentry::ClientInitGuard>> {
    let sentry = config.map(client).transpose()?;

    let event_level = match config.map(|c| c.event_level) {
        Some(SentryEventLevel::Error) => Level::ERROR,
        _ => Level::WARN,
    };
    let sentry_layer = sentry.is_some().then(|| {
        SpanLayer::new()
            .with_filter(LevelFilter::INFO)
            .and_then(BreadcrumbLayer::new().with_filter(filter_fn(move |meta| {
                classify(*meta.level(), event_level) == Report::Breadcrumb
            })))
            .and_then(ErrorLayer::new().with_filter(filter_fn(move |meta| {
                classify(*meta.level(), event_level) == Report::Event
            })))
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            ),
        )
        .with(sentry_layer)
        .init();

    Ok(sentry)
}

/// How a tracing event at some level is reported to Sentry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    /// A Sentry issue.
    Event,
    /// Context attached to the next issue.
    Breadcrumb,
    Ignore,
}

/// `Level` is ordered by verbosity, so `ERROR <= WARN <= INFO`.
fn classify(level: Level, event_level: Level) -> Report {
    match level {
        l if l <= event_level => Report::Event,
        l if l <= Level::INFO => Report::Breadcrumb,
        _ => Report::Ignore,
    }
}

fn client(config: &SentryConfig) -> anyhow::Result<sentry::ClientInitGuard> {
    // Parsed here rather than via `ClientOptions::dsn`, which panics.
    let dsn: sentry::types::Dsn = config
        .dsn
        .parse()
        .context("failed to parse sentry.dsn as a Sentry DSN")?;

    let mut options = sentry::ClientOptions::new()
        .release(
            config
                .release
                .clone()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        )
        .sample_rate(config.sample_rate)
        .send_default_pii(false)
        .attach_stacktrace(true)
        // Don't log user identities or request details:
        .before_send(|mut event: sentry::protocol::Event<'static>| {
            event.user = None;
            event.request = None;
            Some(event)
        });
    options.dsn = Some(dsn);
    if let Some(environment) = config.environment.clone() {
        options = options.environment(environment);
    }

    Ok(sentry::init(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_threshold_reports_warnings_as_issues() {
        assert_eq!(classify(Level::ERROR, Level::WARN), Report::Event);
        assert_eq!(classify(Level::WARN, Level::WARN), Report::Event);
        assert_eq!(classify(Level::INFO, Level::WARN), Report::Breadcrumb);
        assert_eq!(classify(Level::DEBUG, Level::WARN), Report::Ignore);
        assert_eq!(classify(Level::TRACE, Level::WARN), Report::Ignore);
    }

    #[test]
    fn error_threshold_demotes_warnings_to_breadcrumbs() {
        assert_eq!(classify(Level::ERROR, Level::ERROR), Report::Event);
        assert_eq!(classify(Level::WARN, Level::ERROR), Report::Breadcrumb);
        assert_eq!(classify(Level::DEBUG, Level::ERROR), Report::Ignore);
    }
}
