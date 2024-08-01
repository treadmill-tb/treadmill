//! The [`Herd`] is an abstraction over the set of supervisors.
//!
//! It allows tracking which supervisors are connected or disconnected, as well as _reserving_
//! supervisors, which generates a [`SupervisorActorProxy`] that can be used to communicate with the
//! underlying supervisor. Only one [`SupervisorActorProxy`] can be created per underlying
//! supervisor connection.

use crate::model::supervisor::SupervisorModel;
use axum::extract::ws;
use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket};
use axum::Error;
use dashmap::DashMap;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{Arc, Weak};
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot, watch, Mutex, OwnedMutexGuard, RwLock};
use treadmill_rs::api::switchboard::JobStatus;
use treadmill_rs::api::switchboard_supervisor;
use treadmill_rs::api::switchboard_supervisor::{
    Request, ResponseMessage, SupervisorEvent, SupervisorStatus,
};
use treadmill_rs::connector::StartJobMessage;
use uuid::Uuid;

type OutboxMessage = (
    switchboard_supervisor::Message,
    Option<oneshot::Sender<ResponseMessage>>,
);

#[derive(Debug, Error)]
pub enum HerdError {
    #[error("supervisor is already connected")]
    SupervisorAlreadyConnected,
    #[error("supervisor is not connected")]
    SupervisorNotConnected,
    #[error("no such supervisor exists")]
    NoSuchSupervisor,
    #[error("supervisor is already reserved")]
    SupervisorAlreadyReserved,
}
#[derive(Debug)]
enum ConnectedSupervisor {
    Reserved(Arc<SupervisorActor>),
    Idle(Arc<SupervisorActor>),
}
impl ConnectedSupervisor {
    pub fn as_connected(&self) -> &Arc<SupervisorActor> {
        match self {
            ConnectedSupervisor::Reserved(x) | ConnectedSupervisor::Idle(x) => x,
        }
    }
    pub fn make_idle(&mut self) {
        let Self::Reserved(actor) = self else {
            tracing::error!("Can't make_idle a supervisor that isn't reserved");
            return;
        };
        *self = Self::Idle(Arc::clone(actor));
    }
    pub fn make_reserved(&mut self) {
        let Self::Idle(actor) = self else {
            tracing::error!("Can't make_reserved a supervisor that isn't idle");
            return;
        };
        *self = Self::Reserved(Arc::clone(actor));
    }
}
#[derive(Debug)]
enum SupervisorActorWrapper {
    Disconnected,
    Connected(ConnectedSupervisor),
}
impl SupervisorActorWrapper {
    pub fn as_connected(&self) -> Option<&Arc<SupervisorActor>> {
        if let Self::Connected(ref x) = self {
            Some(x.as_connected())
        } else {
            None
        }
    }
}
#[derive(Debug)]
struct HerdInner {
    supervisors: HashMap<Uuid, SupervisorActorWrapper>,
}
impl HerdInner {
    async fn supervisor_connected(
        &mut self,
        model: &SupervisorModel,
        socket: WebSocket,
    ) -> Result<tokio::task::JoinHandle<()>, HerdError> {
        let id = model.id();
        let Some(supervisor) = self.supervisors.get_mut(&id) else {
            // this can only happen if the supervisor was removed in the time after the
            // SupervisorModel was fetched.
            tracing::warn!(
                "Supervisor ({id}) cannot connect as it has been deleted from the registry."
            );
            return Err(HerdError::NoSuchSupervisor);
        };
        if let Some(already_connected) = supervisor.as_connected() {
            if already_connected.check_is_connected().await {
                tracing::error!("Supervisor ({id}) is already connected, refusing connection.");
                return Err(HerdError::SupervisorAlreadyConnected);
            } else {
                // Okay
            }
        }

        let (loop_join, outbox, control_queue, event_receiver) = supervisor_run(id, socket);

        let actor = SupervisorActor {
            outbox,
            event_receiver: Arc::new(Mutex::new(event_receiver)),
            control_queue,
        };
        *supervisor = SupervisorActorWrapper::Connected(ConnectedSupervisor::Idle(Arc::new(actor)));

        Ok(loop_join)
    }
    fn supervisor_disconnected(&mut self, id: Uuid) {
        let Some(actor) = self.supervisors.get_mut(&id) else {
            tracing::error!("Supervisor ({id}) cannot be unreserved: unregistered.");
            return;
        };
        *actor = SupervisorActorWrapper::Disconnected;
    }
    async fn reserve_proxy(
        &mut self,
        id: Uuid,
        herd: Weak<Herd>,
    ) -> Result<SupervisorActorProxy, HerdError> {
        // `id` wasn't present
        // Three cases: nonexistent supervisor, disconnected supervisor, supervisor already reserved
        let Some(supervisor) = self.supervisors.get_mut(&id) else {
            tracing::error!("Can't reserve supervisor ({id}) proxy: no such supervisor.");
            return Err(HerdError::NoSuchSupervisor);
        };
        let actor = match supervisor {
            SupervisorActorWrapper::Disconnected => {
                tracing::error!("Can't reserve supervisor ({id}) proxy: supervisor disconnected.");
                return Err(HerdError::SupervisorNotConnected);
            }
            SupervisorActorWrapper::Connected(ConnectedSupervisor::Reserved(_)) => {
                tracing::error!(
                    "Can't reserve supervisor ({id}) proxy: supervisor already reserved."
                );
                return Err(HerdError::SupervisorAlreadyReserved);
            }
            SupervisorActorWrapper::Connected(
                ref mut connected_supervisor @ ConnectedSupervisor::Idle(_),
            ) => {
                connected_supervisor.make_reserved();
                let ConnectedSupervisor::Idle(actor) = connected_supervisor else {
                    unreachable!()
                };
                actor
            }
        };

        let event_receiver = actor.event_receiver.clone().lock_owned().await;
        let (job_status_watch, event_receiver) = watch_job_status(event_receiver);

        Ok(SupervisorActorProxy {
            supervisor_id: id,
            outbox: actor.outbox.clone(),
            job_status_watch,
            event_receiver,
            herd,
        })
    }
    fn unreserve(&mut self, id: Uuid) {
        let Some(actor) = self.supervisors.get_mut(&id) else {
            tracing::error!("Supervisor ({id}) cannot be unreserved: unregistered.");
            return;
        };
        let SupervisorActorWrapper::Connected(connected_actor) = actor else {
            tracing::warn!("Supervisor ({id}) cannot be unreserved: not connected.");
            return;
        };
        connected_actor.make_idle();
    }
}
pub struct Herd(Arc<RwLock<HerdInner>>);
impl Herd {
    pub async fn supervisor_connected(
        self: &Arc<Self>,
        model: &SupervisorModel,
        socket: WebSocket,
    ) -> Result<(), HerdError> {
        let loop_join = {
            let mut lg = self.0.write().await;
            lg.supervisor_connected(model, socket).await?
        };
        let this = Arc::clone(self);
        let supervisor_id = model.id();
        tokio::spawn(async move {
            if let Err(e) = loop_join.await {
                tracing::error!("Failed to join supervisor runloop: {e}");
            }
            let _ = this.supervisor_disconnected(supervisor_id).await;
        });
        Ok(())
    }
    async fn supervisor_disconnected(&self, id: Uuid) {
        let mut lg = self.0.write().await;
        lg.supervisor_disconnected(id);
    }
    pub async fn reserve_proxy(
        self: &Arc<Self>,
        supervisor_id: Uuid,
    ) -> Result<SupervisorActorProxy, HerdError> {
        let mut lg = self.0.write().await;
        lg.reserve_proxy(supervisor_id, Arc::downgrade(self)).await
    }
    async fn unreserve(&self, supervisor_id: Uuid) {
        let mut lg = self.0.write().await;
        lg.unreserve(supervisor_id);
    }
}

/// Creates a Tokio task that forwards incoming events from `event_receiver`.
/// All incoming events are forwarded to the [`mpsc::UnboundedReceiver`] returned, but job state updates
/// and errors are copied onto the [`watch::Receiver`].
/// When
fn watch_job_status(
    mut event_receiver: OwnedMutexGuard<UnboundedReceiver<SupervisorEvent>>,
) -> (
    watch::Receiver<JobStatus>,
    UnboundedReceiver<SupervisorEvent>,
) {
    let (status_tx, status_rx) = watch::channel(JobStatus::Inactive);
    let (all_tx, all_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(event) = tokio::select! {
            event = event_receiver.recv() => { event }
            // if either the channels close, then that's the signal to exit
            _ = all_tx.closed() => { None }
            _ = status_tx.closed() => { None }
        } {
            match &event {
                SupervisorEvent::UpdateJobState { job_state, .. } => {
                    if status_tx
                        .send(JobStatus::Active {
                            job_state: job_state.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                SupervisorEvent::ReportJobError { error, .. } => {
                    if status_tx
                        .send(JobStatus::Error {
                            job_error: error.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                SupervisorEvent::SendJobConsoleLog { .. } => {}
            }
            if all_tx.send(event).is_err() {
                break;
            }
        }
    });

    (status_rx, all_rx)
}

pub struct SupervisorActorProxy {
    supervisor_id: Uuid,
    outbox: UnboundedSender<OutboxMessage>,
    job_status_watch: watch::Receiver<JobStatus>,
    #[allow(dead_code)]
    event_receiver: UnboundedReceiver<SupervisorEvent>,
    herd: Weak<Herd>,
}
impl Drop for SupervisorActorProxy {
    fn drop(&mut self) {
        // TODO: is this correct?
        if let Some(herd) = self.herd.upgrade() {
            let herd = Arc::clone(&herd);
            let supervisor_id = self.supervisor_id;
            tokio::spawn(async move { herd.unreserve(supervisor_id).await });
        }
    }
}
impl SupervisorActorProxy {
    pub async fn start_job(&self, message: StartJobMessage) -> Option<()> {
        self.outbox
            .send((switchboard_supervisor::Message::StartJob(message), None))
            .ok()
    }
    pub fn job_status_watcher(&self) -> watch::Receiver<JobStatus> {
        self.job_status_watch.clone()
    }
    pub async fn request_supervisor_status(&self) -> Option<SupervisorStatus> {
        let (tx, rx) = oneshot::channel();
        self.outbox
            .send((
                switchboard_supervisor::Message::StatusRequest(Request {
                    request_id: Uuid::new_v4(),
                    message: (),
                }),
                Some(tx),
            ))
            .ok()?;
        rx.await.ok().and_then(|response| {
            let ResponseMessage::StatusResponse(status) = response else {
                tracing::error!("invalid response to StatusRequest: {response:?}");
                return None;
            };
            Some(status)
        })
    }
}

#[derive(Debug)]
struct SupervisorActor {
    outbox: UnboundedSender<OutboxMessage>,
    event_receiver: Arc<Mutex<UnboundedReceiver<SupervisorEvent>>>,
    control_queue: UnboundedSender<ControlRequest>,
}
impl SupervisorActor {
    async fn check_is_connected(&self) -> bool {
        let (sender, receiver) = oneshot::channel();
        if let Err(e) = self
            .control_queue
            .send((ConnectionControlRequest::CheckConnection, sender))
        {
            tracing::error!("Failed to send control request: CheckConnection: {e}");
            return false;
        }
        match receiver.await {
            Ok(response) => match response {
                ConnectionControlResponse::CheckConnection(conn_stat) => match conn_stat {
                    ConnectionStatus::Connected => true,
                    ConnectionStatus::Disconnected => false,
                },
                // _ => unreachable!(),
            },
            Err(e) => {
                tracing::error!("Failed to receiver control response: {e}");
                false
            }
        }
    }
}

/// Returns (join_handle, outbox, ctl, event_queue)
fn supervisor_run(
    supervisor_id: Uuid,
    socket: WebSocket,
) -> (
    tokio::task::JoinHandle<()>,
    UnboundedSender<OutboxMessage>,
    UnboundedSender<ControlRequest>,
    UnboundedReceiver<SupervisorEvent>,
) {
    let (outbox_tx, outbox_rx) = mpsc::unbounded_channel();
    let (event_report_queue_tx, event_report_queue_rx) = mpsc::unbounded_channel();
    let (control_queue_tx, control_queue_rx) = mpsc::unbounded_channel();

    let mut conn = SupervisorConnection {
        supervisor_id,
        outbox: outbox_rx,
        event_report_queue: event_report_queue_tx,
        outstanding_requests: Default::default(),
        control_queue: control_queue_rx,
    };
    let jh = tokio::spawn(async move { conn.run(socket).await });

    (jh, outbox_tx, control_queue_tx, event_report_queue_rx)
}

enum ConnectionControlRequest {
    CheckConnection,
}
enum ConnectionStatus {
    Connected,
    Disconnected,
}
#[non_exhaustive]
enum ConnectionControlResponse {
    CheckConnection(ConnectionStatus),
}
type ControlRequest = (
    ConnectionControlRequest,
    oneshot::Sender<ConnectionControlResponse>,
);
struct SupervisorConnection {
    supervisor_id: Uuid,
    outbox: UnboundedReceiver<OutboxMessage>,
    event_report_queue: UnboundedSender<SupervisorEvent>,
    outstanding_requests: DashMap<Uuid, oneshot::Sender<ResponseMessage>>,
    control_queue: UnboundedReceiver<ControlRequest>,
}
impl SupervisorConnection {
    async fn try_close(
        &self,
        socket: &mut WebSocket,
        supervisor_id: Uuid,
        maybe_cf: Option<CloseFrame<'static>>,
    ) {
        if let Err(e) = socket.send(WsMessage::Close(maybe_cf)).await {
            tracing::error!("Failed to send close frame to supervisor ({supervisor_id}): {e}.");
        }
        // .send(..::Close(..)) already closes the socket, so no need to call .close()
    }

    /// Primary run-loop. This owns the actual WebSocket connection. When this exits, the connection
    /// is considered closed.
    async fn run(&mut self, mut socket: WebSocket) {
        // TODO: need more robust method of error handling
        let supervisor_id = self.supervisor_id;
        loop {
            tokio::select! {
                out = self.outbox.recv() => {
                    if let Some(outbox_message) = out {
                        if self.handle_outgoing_message(&mut socket, outbox_message).await.is_break() {
                            return
                        }
                    }
                }
                r = socket.next() => {
                    if let Some(r) = r {
                        if self.handle_incoming_message(&mut socket, supervisor_id, r).await.is_break() {
                            return
                        }
                    } else {
                        // connection closed without close message
                        break
                    }
                }
                ctl = self.control_queue.recv() => {
                    if let Some((ctl, ret)) = ctl {
                        let resp = self.handle_control_message(ctl, &mut socket).await;
                        let _ = ret.send(resp);
                    }
                }
            }
        }
        // next() -> None - indicates -> connection closed
        tracing::error!("connection closed unexpectedly by supervisor");
    }

    async fn handle_control_message(
        &mut self,
        ctl: ConnectionControlRequest,
        socket: &mut WebSocket,
    ) -> ConnectionControlResponse {
        match ctl {
            ConnectionControlRequest::CheckConnection => {
                // TODO: I think this works as a connection test, but I'm not 100% sure
                if socket
                    .send(WsMessage::Ping(vec![]))
                    .await
                    .inspect_err(|e| {
                        tracing::warn!("Failed to checl connection: {e}");
                    })
                    .is_ok()
                {
                    ConnectionControlResponse::CheckConnection(ConnectionStatus::Disconnected)
                } else {
                    ConnectionControlResponse::CheckConnection(ConnectionStatus::Connected)
                }
            }
        }
    }

    async fn handle_outgoing_message(
        &mut self,
        socket: &mut WebSocket,
        outbox_message: OutboxMessage,
    ) -> ControlFlow<(), ()> {
        let (m, maybe_notify) = outbox_message;
        let id = m.request_id();
        match (id, maybe_notify) {
            (Some(id), Some(notifier)) => {
                // UUID uniqueness assumption
                let _ = self.outstanding_requests.insert(id, notifier);
            }
            (None, None) => {}
            (Some(_), None) => {
                tracing::error!("mismatch: outgoing request does not have attached notifier");
                return ControlFlow::Break(());
            }
            (None, Some(_)) => {
                tracing::error!("mismatch: outgoing non-request message has attached notifier");
                return ControlFlow::Break(());
            }
        }
        let stringified = match serde_json::to_string(&m) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to serialize outgoing message: {e}");
                return ControlFlow::Break(());
            }
        };
        if let Err(e) = socket.send(WsMessage::Text(stringified)).await {
            tracing::error!("failed to send message over websocket: {e}");
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    fn handle_switchboard_message(
        &self,
        m: switchboard_supervisor::Message,
    ) -> ControlFlow<(), ()> {
        let m = match m.to_response_message() {
            Ok(rm) => {
                if let Some((_, notifier)) =
                    self.outstanding_requests.remove(&rm.response_to_request_id)
                {
                    if let Err(_) = notifier.send(rm.message) {
                        tracing::error!(
                            "failed to send response message via outstanding channel: receiver dropped"
                        );
                        todo!("return value");
                    } else {
                        // successful
                        return ControlFlow::Continue(());
                    }
                } else {
                    tracing::error!(
                        "received response to request ({}) that is no longer outstanding",
                        rm.response_to_request_id
                    );
                    todo!("return value");
                }
            }
            Err(x) => x,
        };
        if let switchboard_supervisor::Message::SupervisorEvent(event) = m {
            if let Err(e) = self.event_report_queue.send(event) {
                tracing::error!(
                    "failed to forward supervisor event {:?} through SwitchboardConnector: {e}",
                    e.0
                );
                todo!("return value")
            }
            return ControlFlow::Continue(());
        }
        tracing::error!("switchboard received invalid message: {m:?}");
        ControlFlow::Break(())
    }

    async fn handle_incoming_message(
        &mut self,
        socket: &mut WebSocket,
        supervisor_id: Uuid,
        r: Result<WsMessage, Error>,
    ) -> ControlFlow<(), ()> {
        match r {
            Ok(ws_message) => match ws_message {
                WsMessage::Text(s) => {
                    let m: switchboard_supervisor::Message = match serde_json::from_str(&s) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!("Error deserializing message ({s}) from supervisor ({supervisor_id}): {e}");
                            self.try_close(socket, supervisor_id, None).await;
                            return ControlFlow::Break(());
                        }
                    };
                    // TODO: control flow -> allow errors to break the loop?
                    self.handle_switchboard_message(m)
                }
                WsMessage::Binary(_) => {
                    tracing::error!("Received binary message from supervisor ({supervisor_id})");
                    self.try_close(socket, supervisor_id, None).await;
                    return ControlFlow::Break(());
                }
                WsMessage::Ping(_) => {
                    tracing::trace!("websocket PING'd");
                    ControlFlow::Continue(())
                }
                WsMessage::Pong(_) => {
                    tracing::trace!("websocket PONG'd");
                    ControlFlow::Continue(())
                }
                WsMessage::Close(maybe_cf) => {
                    let cm = maybe_cf
                        .clone()
                        .map(|cf| format!("({}) {}", cf.code, cf.reason))
                        .unwrap_or("<no close frame>".to_string());
                    if maybe_cf
                        .map(|cf| cf.code == ws::close_code::NORMAL)
                        .unwrap_or(false)
                    {
                        tracing::info!("Websocket closed by supervisor ({supervisor_id}): {cm}");
                    } else {
                        tracing::error!("Websocket closed by supervisor ({supervisor_id}): {cm}");
                    }
                    return ControlFlow::Break(());
                }
            },
            Err(e) => {
                tracing::error!("Error receiving from socket: {e}, closing.");
                self.try_close(socket, self.supervisor_id, None).await;
                return ControlFlow::Break(());
            }
        }
    }
}
