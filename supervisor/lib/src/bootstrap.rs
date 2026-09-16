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

/// Types of stop requests, either terminate or restart post job completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// Exit once the coordinator has removed the current job, triggered by
    /// `SIGUSR1`, idempotent.
    AfterJob,
    /// Terminate the job, remove it, and exit. Triggered by `SIGINT`.
    StopJob,
}

/// Behavior for repeated signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnRepeat {
    /// Run `action` again and keep going.
    Reassert,
    /// Exit with `128 + signo`.
    Exit,
}

/// Run `action` every time `kind` arrives, logging `what`, and apply
/// `on_repeat` from the second signal on.
fn on_signal(
    kind: SignalKind,
    on_repeat: OnRepeat,
    what: &'static str,
    action: impl Fn() + Send + 'static,
) {
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
                match on_repeat {
                    OnRepeat::Exit => {
                        event!(Level::WARN, ?kind, "Received again, exiting immediately");
                        std::process::exit(128 + kind.as_raw_value());
                    }
                    OnRepeat::Reassert => event!(Level::INFO, ?kind, "Received again: {}", what),
                }
            } else {
                acted = true;
                event!(Level::INFO, ?kind, "{}", what);
            }
            action();
        }
    });
}

/// Drive `runner` off `connector` until the process is asked to stop, then take
/// down whatever job is left.
pub async fn serve<B: JobBackend>(
    connector: Arc<dyn SupervisorConnector>,
    runner: Arc<JobRunner<B>>,
    command_rx: mpsc::Receiver<CoordCommand>,
    stop_signal: StopSignal,
    on_disconnect: OnDisconnect,
) {
    match stop_signal {
        StopSignal::AfterJob => on_signal(
            SignalKind::user_defined1(),
            OnRepeat::Reassert,
            "Exiting once the coordinator has removed the current job",
            {
                let connector = connector.clone();
                move || connector.request_shutdown()
            },
        ),
        StopSignal::StopJob => on_signal(
            SignalKind::user_defined1(),
            OnRepeat::Reassert,
            "Ignoring: no coordinator to wait for",
            || (),
        ),
    }

    if stop_signal == StopSignal::StopJob {
        on_signal(
            SignalKind::interrupt(),
            OnRepeat::Exit,
            "Stopping the job and shutting the supervisor down",
            {
                let connector = connector.clone();
                move || connector.request_shutdown()
            },
        );
    }

    let stop = CancellationToken::new();
    on_signal(
        SignalKind::terminate(),
        OnRepeat::Exit,
        "Shutting the supervisor down",
        {
            let stop = stop.clone();
            move || stop.cancel()
        },
    );

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
