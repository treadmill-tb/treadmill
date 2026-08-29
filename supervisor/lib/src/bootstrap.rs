//! Wiring a supervisor's connector, job runner, and signals together.
//!
//! Everything here is platform-independent: a supervisor's `main` builds its
//! [`JobBackend`](crate::job::JobBackend), picks a connector, and hands both to
//! [`serve`], which owns the process's shutdown semantics from there.

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
    /// Reconnect after [`RECONNECT_DELAY`] — a supervisor serving a remote
    /// coordinator outlives any one connection to it.
    Reconnect,
    /// Give up. There is nothing to reconnect to.
    Exit,
}

/// Run `action` the first time `kind` arrives.
///
/// A repeat of the signal gives up and exits, so a shutdown that cannot make
/// progress — a coordinator that never removes the job, a workload that will
/// not die — is still escapable.
fn on_signal(kind: SignalKind, action: impl Fn() + Send + 'static) {
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
///   `SIGINT` when there is not) asks the connector to stop serving. A
///   connector that drains keeps serving the job it holds until the coordinator
///   removes it, so the process exits between jobs.
/// - `SIGTERM` does not wait for anyone: it stops serving and takes the running
///   job down with it.
///
/// `Ok(())` from `run()` means the connector is done serving, not that the slot
/// is empty: the ws connector only returns it once the switchboard has removed
/// the job, but the local connector returns as soon as its one job reports
/// `Terminated`, leaving the retained terminal record behind. So every path out
/// of the loop ends in [`JobRunner::shutdown`], which terminates and releases
/// whatever is still there rather than orphaning it along with the process.
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
