use crate::service::herd::Reservation;
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot, watch};
use treadmill_rs::api::switchboard::{ExitStatus, JobStatus};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum KanbanError {
    #[error("job with the specified ID already exists")]
    AlreadyExists,
    #[error("no such job")]
    InvalidJob,
}

// A queued job doesn't really need a great deal of information.
// We need:
// - to know which supervisors it is matched to
// - the watchdog to know when it times out
// - the watchdog to notify the scheduler when it times out
pub struct QueuedJob {
    matched_supervisors: BTreeSet<Uuid>,
    times_out_at: DateTime<Utc>,
    // Send on this to notify the service's queue timeout watchdog that the job is leaving early
    pub exit_notifier: oneshot::Sender<()>,
}

// An active job likewise does not require a great deal of bookkeeping.
// For convenience's sake, we store the ID of the supervisor it's running on.
// Additionally, we really want to know if we have a reservation or not.
// The timeout mechanism for active jobs works differently - unlike the batch timeout of queued
// jobs, active jobs are individually watched. Therefore, we do not need to store the time at
// which the job expires, unlike in [`QueuedJob`].
// Similarly, no timeout notification is needed because a task already exists, with the
// necessary logic to handle termination events.
pub struct ActiveJob {
    /// For convenience's sake, store the ID of the supervisor the job is running on.
    pub running_on_supervisor_id: Uuid,

    /// Current reservation the job is running on, if there is one.
    pub current_reservation: Option<Reservation>,

    /// Whenever a new reservation is created, the UnboundedReceiver<JobStatus> should be sent here
    pub job_status_receiver_sender: UnboundedSender<UnboundedReceiver<JobStatus>>,

    /// Used to notify the termination watchdog that the job is being canceled
    pub stop_tx: Option<oneshot::Sender<ExitStatus>>,
}

pub struct Kanban {
    queued_jobs: BTreeMap<Uuid, QueuedJob>,
    active_jobs: BTreeMap<Uuid, ActiveJob>,
}

impl Kanban {
    pub fn new() -> Self {
        Self {
            queued_jobs: Default::default(),
            active_jobs: Default::default(),
        }
    }

    pub fn add_queued_job(
        &mut self,
        job_id: Uuid,
        matched_supervisors: BTreeSet<Uuid>,
        queued_at: DateTime<Utc>,
        queue_timeout: TimeDelta,
    ) -> Result<oneshot::Receiver<()>, KanbanError> {
        if self.queued_jobs.contains_key(&job_id) {
            return Err(KanbanError::AlreadyExists);
        }
        if self.active_jobs.contains_key(&job_id) {
            return Err(KanbanError::AlreadyExists);
        }
        let (exit_tx, exit_rx) = oneshot::channel();
        let times_out_at = queued_at + queue_timeout;
        self.queued_jobs.insert(
            job_id,
            QueuedJob {
                matched_supervisors,
                times_out_at,
                exit_notifier: exit_tx,
            },
        );

        Ok(exit_rx)
    }

    pub fn add_active_job(
        &mut self,
        job_id: Uuid,
        supervisor_id: Uuid,
        mut reservation: Option<Reservation>,
        stop_tx: oneshot::Sender<ExitStatus>,
    ) -> Result<(UnboundedReceiver<JobStatus>, watch::Sender<()>), KanbanError> {
        if self.queued_jobs.contains_key(&job_id) {
            return Err(KanbanError::AlreadyExists);
        }
        if self.active_jobs.contains_key(&job_id) {
            return Err(KanbanError::AlreadyExists);
        }

        // Launch channel reloader! Very important.
        // Will close when `close_tx` send a message or drops, or jsr_tx drops.
        let (jsr_tx, job_status_rx, close_tx) = launch_channel_reloader();

        if let Some(reservation) = reservation.as_mut() {
            // preload the channel reloader with the JSR from the reservation.
            let _ = jsr_tx.send(reservation.take_job_status_receiver().unwrap());
        }

        self.active_jobs.insert(
            job_id,
            ActiveJob {
                running_on_supervisor_id: supervisor_id,
                current_reservation: reservation,
                job_status_receiver_sender: jsr_tx,
                stop_tx: Some(stop_tx),
            },
        );

        Ok((job_status_rx, close_tx))
    }

    pub fn activate(
        &mut self,
        job_id: Uuid,
        supervisor_id: Uuid,
        reservation: Reservation,
        stop_tx: oneshot::Sender<ExitStatus>,
    ) -> Result<(UnboundedReceiver<JobStatus>, watch::Sender<()>), KanbanError> {
        let queued_job = self.remove_queued_job(job_id)?;
        // Disarm the queue timeout watchdog
        let _ = queued_job.exit_notifier.send(());
        self.add_active_job(job_id, supervisor_id, Some(reservation), stop_tx)
    }

    pub fn remove_queued_job(&mut self, job_id: Uuid) -> Result<QueuedJob, KanbanError> {
        // if !self.queued_jobs.contains_key(&job_id) {
        //     return Err(KanbanError::InvalidJob);
        // }
        self.queued_jobs
            .remove(&job_id)
            .ok_or(KanbanError::InvalidJob)
    }

    pub fn remove_active_job(&mut self, job_id: Uuid) -> Result<ActiveJob, KanbanError> {
        self.active_jobs
            .remove(&job_id)
            .ok_or(KanbanError::InvalidJob)
    }

    pub fn get_active_job_on(&self, supervisor_id: Uuid) -> Option<Uuid> {
        self.active_jobs
            .iter()
            .find(|&(_job_id, active_job)| active_job.running_on_supervisor_id == supervisor_id)
            .map(|(&job_id, _active_job)| job_id)
    }

    pub fn get_active_job(&mut self, job_id: Uuid) -> Option<&mut ActiveJob> {
        self.active_jobs.get_mut(&job_id)
    }

    pub fn get_queued_jobs(&self) -> Vec<(Uuid, &BTreeSet<Uuid>)> {
        self.queued_jobs
            .iter()
            .map(|(i, v)| (*i, &v.matched_supervisors))
            .collect()
    }

    pub fn queued_job_exists(&self, job_id: Uuid) -> bool {
        self.queued_jobs.contains_key(&job_id)
    }
}

/// Creates a task that performs _channel reloading_.
///
/// The [`Service`] needs access to an uninterrupted stream of job status events. However, due to
/// supervisor disconnections and reconnections, directly using a connection-derived channel is
/// insufficient. Therefore, we have a _channel of channels_: when a supervisor connects, a receiver
/// is sent to the channel of channels, and all the events that occur during the connection are sent
/// to this channel.
///
/// The _channel reloading_ task drains all the events from all the channels in the channel of
/// channels (in the original order), and then forwards them into a new, single channel, that is
/// unaffected by supervisor disconnections and reconnections.
///
/// Further, a third channel is created so that the task can be forcibly shut down should this
/// become necessary. Specifically, this is a `watch::Sender<()>`; when a value is sent to this
/// channel, or the channel drops, the channel reloading task will forward any remaining events it
/// has access to, and then exit immediately.
fn launch_channel_reloader() -> (
    UnboundedSender<UnboundedReceiver<JobStatus>>,
    UnboundedReceiver<JobStatus>,
    watch::Sender<()>,
) {
    let (jsr_tx, mut jsr_rx) = mpsc::unbounded_channel::<UnboundedReceiver<JobStatus>>();
    let (job_status_tx, job_status_rx) = mpsc::unbounded_channel::<JobStatus>();
    let (close_tx, mut close_rx) = watch::channel(());

    tokio::spawn(async move {
        loop {
            let channel = tokio::select! {
                channel = jsr_rx.recv() => channel,
                _ = close_rx.changed() => {
                    // Since changed() fired before recv() did, we've no access to any pending
                    // updates. Therefore, close immediately.
                    break
                },
            };
            // If jsr_rx.recv() gave us None, then the channel is closed. Exit.
            let Some(mut channel) = channel else { break };

            loop {
                let update = tokio::select! {
                    update = channel.recv() => update,
                    _ = close_rx.changed() => {
                        // Again, since changed() fired before recv() did, we've no access to
                        // any pending updates.

                        // We don't know whether there are any other channels in the JSR_RX, so
                        // we only break one loop. We want the .changed() at the head of the
                        // outer loop to still fire, so we mark the value as changed
                        // artificially.
                        close_rx.mark_changed();
                        break
                    }
                };
                // If channel.recv() gave us None, then the channel is closed. Exit.
                let Some(update) = update else { break };
                if job_status_tx.send(update).is_err() {
                    // if we're unable to send values, then the receiver has dropped, and we
                    // should stop trying.
                    break;
                }
            }
        }
    });

    (jsr_tx, job_status_rx, close_tx)
}
