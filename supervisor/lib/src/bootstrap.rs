//! Convenience infrastructure for setting up a supervisor's connector, job
//! runner, and signal handling.

use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Level, event};

use treadmill_rs::connector::{CoordCommand, SupervisorConnector};

use crate::job::{JobBackend, JobRunner};

/// Depth of the coordinator's command mailbox.
pub const COORD_MAILBOX_CAPACITY: usize = 8;

/// How long a connector that lost its coordinator waits before retrying.
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// What to do when a connector's `run()` reports it lost its coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDisconnect {
    /// Reconnect after [`RECONNECT_DELAY`]: a supervisor serving a remote
    /// coordinator outlives any one connection to it.
    Reconnect,
    /// Give up. There is nothing to reconnect to.
    Exit,
}

/// Run `action` the first time `kind` arrives.
///
/// A repeat of the signal gives up and exits, so a shutdown that cannot make
/// progress (a coordinator that never removes the job, a workload that will not
/// die) doesn't prevent shutdown.
///
/// TODO: this is tricky! It'll break when we have two supervisor updates while
/// running a job, and systemd sends two SIGHUPs on ExecReload...
fn on_signal(kind: SignalKind, action: impl Fn() + Send + 'static) {
    // Create the signal listener:
    let mut signals = match signal(kind) {
        Ok(signals) => signals,
        Err(e) => {
            event!(
                Level::WARN,
                ?kind,
                error = ?e,
                "Cannot listen for this signal; it will not shut the supervisor down",
            );
            return;
        }
    };

    // Listen on the signal without blocking the current task:
    tokio::spawn(async move {
        let mut acted = false;
        while signals.recv().await.is_some() {
            if acted {
                event!(Level::WARN, ?kind, "Received again, exiting immediately");
                std::process::exit(128 + kind.as_raw_value());
            }
            acted = true;
            event!(Level::INFO, ?kind, "Shutting the supervisor down");
            action();
        }
    });
}

/// Drive `runner` off `connector` until the process is asked to stop, then take
/// down whatever job is left.
///
/// Two signals end the loop, and they mean different things:
///
/// - `drain_signal` (`SIGHUP` when there is a coordinator to drain against,
///   `SIGINT` when there is not) asks the connector to stop serving.
///
///   A connector that drains keeps serving the job it holds until the
///   coordinator removes it, so the process exits between jobs (but not before
///   terminate was received!)
///
/// - `SIGTERM` does not wait for anyone: it stops serving and takes the running
///   job down with it.
pub async fn serve<B: JobBackend>(
    connector: Arc<dyn SupervisorConnector>,
    runner: Arc<JobRunner<B>>,
    command_rx: mpsc::Receiver<CoordCommand>,
    drain_signal: SignalKind,
    on_disconnect: OnDisconnect,
) {
    on_signal(drain_signal, {
        let connector = connector.clone();
        move || connector.request_shutdown()
    });

    let stop = CancellationToken::new();
    on_signal(SignalKind::terminate(), {
        let stop = stop.clone();
        move || stop.cancel()
    });

    let commands = tokio::spawn({
        let runner = runner.clone();
        async move { runner.run(command_rx).await }
    });

    loop {
        let run = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            run = connector.run() => run,
        };

        match run {
            Ok(()) => {
                event!(Level::INFO, "Connector exited, shutting down supervisor...");
                break;
            }
            Err(()) if on_disconnect == OnDisconnect::Reconnect => {
                event!(
                    Level::WARN,
                    "Connector exited with an error, reconnecting in {:?}...",
                    RECONNECT_DELAY,
                );
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(()) => {
                event!(Level::WARN, "Connector exited with an error.");
                break;
            }
        }
    }

    commands.abort();
    runner.shutdown().await;
}
