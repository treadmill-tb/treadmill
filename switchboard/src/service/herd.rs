use crate::service::socket_connection::OutboxMessage;
use axum::extract::ws::WebSocket;
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc, oneshot};
use tokio::task::JoinHandle;
use treadmill_rs::api::switchboard_supervisor;
use treadmill_rs::api::switchboard_supervisor::{
    ReportedSupervisorStatus, Request, ResponseMessage, SocketConfig, SupervisorEvent,
    SupervisorJobEvent,
};
use treadmill_rs::connector::{StartJobMessage, StopJobMessage};
use uuid::Uuid;

enum SupervisorCondition {
    Disconnected,
    Idle(Arc<Mutex<ConnectedSupervisor>>),
    Reserved(Arc<Mutex<ConnectedSupervisor>>),
}
impl Debug for SupervisorCondition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                SupervisorCondition::Disconnected => {
                    "SupervisorCondition::Disconnected"
                }
                SupervisorCondition::Idle(_) => {
                    "SupervisorCondition::Idle"
                }
                SupervisorCondition::Reserved(_) => {
                    "SupervisorCondition::Reserved"
                }
            }
        )
    }
}

pub struct ConnectedSupervisor {
    outbox_sender: UnboundedSender<OutboxMessage>,
    // for debugging
    #[allow(dead_code)]
    event_stream: UnboundedReceiver<SupervisorEvent>,
    job_status_receiver: Arc<Mutex<UnboundedReceiver<SupervisorJobEvent>>>,
}
impl ConnectedSupervisor {
    pub async fn request_supervisor_status(&self) -> Result<ReportedSupervisorStatus, HerdError> {
        let (tx, rx) = oneshot::channel();
        self.outbox_sender
            .send((
                switchboard_supervisor::Message::StatusRequest(Request {
                    request_id: Uuid::new_v4(),
                    message: (),
                }),
                Some(tx),
            ))
            .map_err(|_| HerdError::NotConnected)?;
        let ResponseMessage::StatusResponse(status) =
            rx.await.map_err(|_| HerdError::NotConnected)?
        else {
            panic!(
                "Received incorrect but type-safe response from supervisor in response to status request"
            );
        };
        Ok(status)
    }
    pub fn stop_job(&self, stop_job_message: StopJobMessage) -> Result<(), HerdError> {
        self.outbox_sender
            .send((
                switchboard_supervisor::Message::StopJob(stop_job_message),
                None,
            ))
            .map_err(|_| HerdError::NotConnected)
    }
}

#[derive(Debug, Error)]
pub enum ReservationError {
    #[error("Supervisor disconnected")]
    Disconnected,
}
pub struct Reservation {
    // for debugging
    #[allow(dead_code)]
    supervisor_id: Uuid,
    outbox_sender: UnboundedSender<OutboxMessage>,
    job_status_receiver: Option<UnboundedReceiver<SupervisorJobEvent>>,
    // not currently used; phased out in favor of reading directly from the database instead of
    // risking race connnections (in the general case)
    // #[allow(dead_code)]
    // job_status_watch: watch::Receiver<JobStatus>,
}
impl Reservation {
    pub fn start_job(&self, start_job_message: StartJobMessage) -> Result<(), ReservationError> {
        self.outbox_sender
            .send((
                switchboard_supervisor::Message::StartJob(start_job_message),
                None,
            ))
            .map_err(|_| ReservationError::Disconnected)
    }
    pub fn stop_job(&self, stop_job_message: StopJobMessage) -> Result<(), ReservationError> {
        self.outbox_sender
            .send((
                switchboard_supervisor::Message::StopJob(stop_job_message),
                None,
            ))
            .map_err(|_| ReservationError::Disconnected)
    }
    pub fn take_job_status_receiver(&mut self) -> Option<UnboundedReceiver<SupervisorJobEvent>> {
        self.job_status_receiver.take()
    }
}

#[derive(Debug, Error)]
pub enum HerdError {
    #[error("The requested supervisor is not registered.")]
    NotRegistered,
    #[error("The requested supervisor is already registered.")]
    AlreadyRegistered,
    #[error("The requested supervisor is not connected.")]
    NotConnected,
    #[error("The requested supervisor is not in the expected condition.")]
    InvalidCondition,
}

async fn build_reservation(
    supervisor_id: Uuid,
    connected_supervisor: Arc<Mutex<ConnectedSupervisor>>,
) -> Reservation {
    let outbox_sender;
    let job_status_receiver;
    {
        let lg = connected_supervisor.lock().await;
        outbox_sender = lg.outbox_sender.clone();
        job_status_receiver = lg.job_status_receiver.clone().lock_owned().await;
    }
    let (job_status_receiver /*job_status_watch*/,) = watch_latest_job_status(job_status_receiver);

    Reservation {
        supervisor_id,
        outbox_sender,
        job_status_receiver: Some(job_status_receiver),
    }
}

#[derive(Debug)]
pub struct Herd {
    supervisors: BTreeMap<Uuid, SupervisorCondition>,
    tag_sets: BTreeMap<Uuid, BTreeSet<String>>,
}

impl Herd {
    #[tracing::instrument(skip(self))]
    pub(crate) fn is_supervisor_connected(&self, supervisor_id: Uuid) -> bool {
        if let Some(cond) = self.supervisors.get(&supervisor_id) {
            match cond {
                SupervisorCondition::Disconnected => {
                    tracing::debug!("Checking connection; current state: disconnected")
                }
                SupervisorCondition::Idle(_) => {
                    tracing::debug!("Checking connection; current state: idle")
                }
                SupervisorCondition::Reserved(_) => {
                    tracing::debug!("Checking connection; current state: reserved")
                }
            }
            !matches!(cond, SupervisorCondition::Disconnected)
        } else {
            tracing::error!("Can't check connection: no such supervisor");
            false
        }
    }
}

impl Herd {
    pub fn list_supervisors(&self) -> Vec<Uuid> {
        self.supervisors.keys().copied().collect()
    }
}

impl Default for Herd {
    fn default() -> Self {
        Self::new()
    }
}

impl Herd {
    /// Create an empty Herd.
    pub fn new() -> Self {
        Self {
            supervisors: BTreeMap::new(),
            tag_sets: BTreeMap::new(),
        }
    }

    pub fn currently_idle(&self) -> Vec<Uuid> {
        self.supervisors
            .iter()
            .filter(|(_, v)| matches!(v, SupervisorCondition::Idle(_)))
            .map(|(i, _)| *i)
            .collect()
    }

    pub fn tag_sets(&self) -> &BTreeMap<Uuid, BTreeSet<String>> {
        &self.tag_sets
    }

    pub fn assert_registered(&self, supervisor_id: Uuid) -> Result<(), HerdError> {
        self.supervisors
            .contains_key(&supervisor_id)
            .then_some(())
            .ok_or(HerdError::NotRegistered)
    }

    /// Reserve the supervisor for use by a particular job, producing a [`Reservation`] through
    /// which the requested supervisor may be interacted with.
    ///
    /// # Errors
    ///
    /// Will return an error if the supervisor is not registered, not connected, or already reserved.
    pub async fn reserve_supervisor(
        &mut self,
        supervisor_id: Uuid,
    ) -> Result<Reservation, HerdError> {
        let Entry::Occupied(mut occupied) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::NotRegistered);
        };
        let connected_supervisor = match occupied.get() {
            SupervisorCondition::Disconnected => {
                return Err(HerdError::NotConnected);
            }
            SupervisorCondition::Reserved(_) => {
                return Err(HerdError::InvalidCondition);
            }
            SupervisorCondition::Idle(connected_supervisor) => Arc::clone(connected_supervisor),
        };

        let reservation = build_reservation(supervisor_id, Arc::clone(&connected_supervisor)).await;
        occupied.insert(SupervisorCondition::Reserved(connected_supervisor));

        Ok(reservation)
    }

    /// Un-reserve a supervisor.
    ///
    /// Note that for the use cases this serves, it's not necessarily an error if the supervisor
    /// can't be unreserved due to disconnection, and as such, error values should be checked by any
    /// downstream users.
    ///
    /// # Errors
    ///
    /// Will return an error if the supervisor is not registered, not connected, or not reserved.
    pub fn unreserve_supervisor(&mut self, supervisor_id: Uuid) -> Result<(), HerdError> {
        let Entry::Occupied(mut occupied) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::NotRegistered);
        };
        let connected_supervisor = match occupied.get() {
            SupervisorCondition::Disconnected => return Err(HerdError::NotConnected),
            SupervisorCondition::Idle(_) => return Err(HerdError::InvalidCondition),
            SupervisorCondition::Reserved(connected_supervisor) => Arc::clone(connected_supervisor),
        };
        occupied.insert(SupervisorCondition::Idle(connected_supervisor));

        Ok(())
    }

    /// Mark a supervisor as connected.
    ///
    /// # Errors
    ///
    /// Will return an error if the supervisor is not registered, or is not currently marked as
    /// disconnected.
    pub fn supervisor_connected(
        &mut self,
        supervisor_id: Uuid,
        connected_supervisor: Arc<Mutex<ConnectedSupervisor>>,
    ) -> Result<(), HerdError> {
        let Entry::Occupied(mut occupied) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::NotRegistered);
        };
        if !matches!(occupied.get(), SupervisorCondition::Disconnected) {
            return Err(HerdError::InvalidCondition);
        }
        occupied.insert(SupervisorCondition::Idle(connected_supervisor));

        Ok(())
    }

    /// Mark a supervisor as connected.
    ///
    /// # Errors
    ///
    /// Will return an error if the supervisor is not registered, or is not currently marked as
    /// disconnected.
    pub async fn reserved_supervisor_connected(
        &mut self,
        supervisor_id: Uuid,
        connected_supervisor: Arc<Mutex<ConnectedSupervisor>>,
    ) -> Result<Reservation, HerdError> {
        let Entry::Occupied(mut occupied) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::NotRegistered);
        };
        if !matches!(occupied.get(), SupervisorCondition::Disconnected) {
            return Err(HerdError::InvalidCondition);
        }
        let reservation = build_reservation(supervisor_id, Arc::clone(&connected_supervisor)).await;
        occupied.insert(SupervisorCondition::Reserved(connected_supervisor));

        Ok(reservation)
    }

    /// Mark a supervisor as disconnected.
    ///
    /// # Errors
    ///
    /// Will return an error if the supervisor is not registered, or is already marked as
    /// disconnected.
    pub fn supervisor_disconnected(
        &mut self,
        supervisor_id: Uuid,
    ) -> Result<Arc<Mutex<ConnectedSupervisor>>, HerdError> {
        let Entry::Occupied(mut occupied) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::NotRegistered);
        };
        match occupied.insert(SupervisorCondition::Disconnected) {
            SupervisorCondition::Disconnected => Err(HerdError::NotConnected),
            SupervisorCondition::Idle(connected_supervisor)
            | SupervisorCondition::Reserved(connected_supervisor) => Ok(connected_supervisor),
        }
    }

    /// Register a supervisor with the herd.
    ///
    /// # Errors
    ///
    /// Returns an error if the supervisor is already registered.
    pub fn register_supervisor(
        &mut self,
        supervisor_id: Uuid,
        tag_set: BTreeSet<String>,
    ) -> Result<(), HerdError> {
        let Entry::Vacant(vacant) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::AlreadyRegistered);
        };
        vacant.insert(SupervisorCondition::Disconnected);
        self.tag_sets.insert(supervisor_id, tag_set);

        Ok(())
    }

    /// Unregister a supervisor with the herd. Note that this will always run as long as the
    /// supervisor is registered, without any regard to the supervisor's current state.
    ///
    /// # Errors
    ///
    /// Returns an error if the supervisor is not registered.
    pub fn unregister_supervisor(&mut self, supervisor_id: Uuid) -> Result<(), HerdError> {
        let Entry::Occupied(occupied) = self.supervisors.entry(supervisor_id) else {
            return Err(HerdError::NotRegistered);
        };
        occupied.remove();
        Ok(())
    }

    /// Actively poll the supervisor for its status.
    ///
    /// # Errors
    ///
    /// Returns an error if the supervisor is not registered, or not connected.
    pub async fn get_supervisor_status(
        &mut self,
        supervisor_id: Uuid,
    ) -> Result<ReportedSupervisorStatus, HerdError> {
        let supervisor_condition = self
            .supervisors
            .get(&supervisor_id)
            .ok_or(HerdError::NotRegistered)?;
        let connected_supervisor = match supervisor_condition {
            SupervisorCondition::Disconnected => {
                return Err(HerdError::NotConnected);
            }
            SupervisorCondition::Idle(connected_supervisor)
            | SupervisorCondition::Reserved(connected_supervisor) => {
                Arc::clone(connected_supervisor)
            }
        };
        // Note: need to create an intermediate binding, ere `connected_supervisor` be overly
        // short-lived and risk a wroth compiler.
        let lg = connected_supervisor.lock().await;
        lg.request_supervisor_status().await
    }
}

/// Create a [`ConnectedSupervisor`] from a supervisor ID and a [`WebSocket`] connection.
pub fn prepare_supervisor_connection(
    supervisor_id: Uuid,
    web_socket: WebSocket,
    config: SocketConfig,
) -> (JoinHandle<()>, ConnectedSupervisor) {
    // Start a task that takes care of the sending and receiving messages, as well as monitoring for
    // the closure of the socket-which event will trigger the completion of the
    // `runloop_join_handle` Future.
    let (runloop_join_handle, outbox_sender, event_receiver) =
        super::socket_connection::supervisor_run_loop(supervisor_id, web_socket, config);

    // Jobs are interested in the status updates received from the supervisor, but not any other
    // kinds of events. Most saliently, they are only interested in the most recent status.
    // Therefore, we introduce a splitter onto the supervisor event stream that performs this
    // filtering.
    let (job_status_receiver, duplicated_event_stream) = extract_job_events(event_receiver);

    let connected_supervisor = ConnectedSupervisor {
        outbox_sender,
        event_stream: duplicated_event_stream,
        job_status_receiver: Arc::new(Mutex::new(job_status_receiver)),
    };

    (runloop_join_handle, connected_supervisor)
}

/// Copies the input `OwnedMutexGuard<UnboundedReceiver<JobStatus>>` onto an [`UnboundedReceiver`]
/// as well as a [`watch::Receiver`], so that both the entire backlog of values _and_ the latest
/// value are conveniently available.
///
/// XXX(mc): currently only duplicates event stream.
fn watch_latest_job_status(
    mut event_receiver: OwnedMutexGuard<UnboundedReceiver<SupervisorJobEvent>>,
) -> (
    UnboundedReceiver<SupervisorJobEvent>, /*watch::Receiver<JobStatus>*/
) {
    let (dupe_tx, dupe_rx) = mpsc::unbounded_channel();
    // let (watch_tx, watch_rx) = watch::channel(JobStatus {
    //     state: JobState::Queued,
    //     at_time: Utc::now(),
    // });

    tokio::spawn(async move {
        while let Some(event) = tokio::select! {
            event = event_receiver.recv() => { event }
            _ = dupe_tx.closed() => { None }
            // _ = watch_tx.closed() => { None }
        } {
            // if watch_tx.send(event.clone()).is_err() {
            //     return;
            // }
            if dupe_tx.send(event.clone()).is_err() {
                return;
            }
        }
    });

    (dupe_rx /*watch_rx*/,)
}

/// Creates a Tokio task that forwards incoming events from `event_receiver`.
/// All incoming events are forwarded to the `UnboundedReceiver<SupervisorEvent>` returned, but job
/// state updates and errors are copied onto the `UnboundedReceiver<JobStatus>`.
/// When either of the receivers is dropped, the spawned task will complete and exit, dropping the
/// senders for both of the returned `Receiver`s.
fn extract_job_events(
    mut event_receiver: UnboundedReceiver<SupervisorEvent>,
) -> (
    UnboundedReceiver<SupervisorJobEvent>,
    UnboundedReceiver<SupervisorEvent>,
) {
    let (status_tx, status_rx) = mpsc::unbounded_channel();
    let (all_tx, all_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(event) = tokio::select! {
            event = event_receiver.recv() => { event }
            // if either the channels close, then that's the signal to exit
            _ = all_tx.closed() => { None }
            _ = status_tx.closed() => { None }
        } {
            match &event {
                SupervisorEvent::JobEvent { job_id: _, event } => {
                    if status_tx.send(event.clone()).is_err() {
                        break;
                    }
                }
            }
            if all_tx.send(event).is_err() {
                break;
            }
        }
    });

    (status_rx, all_rx)
}
