use crate::model::supervisor::SupervisorModel;
use axum::extract::ws;
use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket};
use axum::Error;
use dashmap::{DashMap, DashSet};
use futures_util::StreamExt;
use std::ops::ControlFlow;
use std::sync::{Arc, Weak};
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, Mutex};
use treadmill_rs::api::switchboard_supervisor::{
    self, Request, ResponseMessage, SupervisorEvent, SupervisorStatus,
};
use treadmill_rs::connector::{StartJobMessage, StopJobMessage};
use uuid::Uuid;

mod auth;
pub mod socket;

#[derive(Debug)]
pub struct ActiveJob {
    //
}
impl ActiveJob {
    //
}

#[derive(Debug)]
pub struct SupervisorActor {
    #[allow(dead_code)]
    model: SupervisorModel,
    agent: SupervisorAgent,
    job: Mutex<Option<Weak<ActiveJob>>>,
}

#[derive(Debug, Error)]
pub enum HerdError {
    /// Specified supervisor is not currently connected (this makes no distinction between whether
    /// there exists a such supervisor, and it simply isn't connected, or whether there is simply no
    /// such supervisor at all, which being nonexistent obviously cannot be connected).
    #[error("no such supervisor is connected")]
    InvalidSupervisor,
    /// Supervisor is already working on another job.
    #[error("supervisor already has active job")]
    BusySupervisor,
}

#[derive(Debug)]
pub struct Herd {
    supervisors: DashMap<Uuid, Arc<SupervisorActor>>,
    #[allow(dead_code)]
    idle_set: DashSet<Uuid>,
}

impl Herd {
    pub fn new() -> Herd {
        Self {
            supervisors: DashMap::new(),
            idle_set: DashSet::new(),
        }
    }
}

impl Herd {
    pub async fn add_supervisor(self: Arc<Self>, model: SupervisorModel, socket: WebSocket) {
        let supervisor_id = model.id();
        let (loop_task_jh, outbox, event_receiver) = supervisor_run(supervisor_id, socket).await;
        let actor = SupervisorActor {
            model,
            agent: SupervisorAgent {
                outbox,
                event_receiver: Mutex::new(event_receiver),
            },
            job: Mutex::new(None),
        };
        // assume: UUID uniqueness
        let _ = self.supervisors.insert(supervisor_id, Arc::new(actor));
        // watchdog: the loop_task_jh will finally finish when the underlying socket closes, so at
        // that point, we remove the agent from the `supervisors` map.
        tokio::spawn(async move {
            if let Err(e) = loop_task_jh.await {
                tracing::error!("Failed to join supervisor runloop: {e}");
            }
            let _ = self.supervisors.remove(&supervisor_id);
        });
    }

    pub async fn try_start_job(
        &self,
        job: StartJobMessage,
        on_supervisor_id: Uuid,
    ) -> Result<(), HerdError> {
        let actor = self
            .supervisors
            .get(&on_supervisor_id)
            .ok_or(HerdError::InvalidSupervisor)?;
        let active_job = Arc::new(ActiveJob {
            //-
        });
        {
            let mut job_lg = actor.job.lock().await;
            if job_lg.is_some() {
                return Err(HerdError::BusySupervisor);
            }
            let _ = job_lg.insert(Arc::downgrade(&active_job));
        }
        // if it returns None, then the supervisor connection closed for some reason in the last few
        // milliseconds
        actor
            .agent
            .start_job(job)
            .await
            .ok_or(HerdError::InvalidSupervisor)?;

        Ok(())
    }
}

type OutboxMessage = (
    switchboard_supervisor::Message,
    Option<oneshot::Sender<ResponseMessage>>,
);

/// Returns (join_handle, outbox, event_queue)
async fn supervisor_run(
    supervisor_id: Uuid,
    socket: WebSocket,
) -> (
    tokio::task::JoinHandle<()>,
    UnboundedSender<OutboxMessage>,
    UnboundedReceiver<SupervisorEvent>,
) {
    let (outbox_tx, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_report_queue_tx, event_report_queue_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut swx_conn = SwitchboardConnector {
        supervisor_id,
        outbox: outbox_rx,
        event_report_queue: event_report_queue_tx,
        outstanding: DashMap::new(),
    };
    let jh = tokio::spawn(async move { swx_conn.run(socket).await });

    (jh, outbox_tx, event_report_queue_rx)
}

#[derive(Debug)]
pub struct SupervisorAgent {
    outbox: UnboundedSender<OutboxMessage>,
    // needs &mut
    event_receiver: Mutex<UnboundedReceiver<SupervisorEvent>>,
}
impl SupervisorAgent {
    pub async fn next_event(&self) -> Option<SupervisorEvent> {
        self.event_receiver.lock().await.recv().await
    }

    pub async fn start_job(&self, message: StartJobMessage) -> Option<()> {
        self.outbox
            .send((switchboard_supervisor::Message::StartJob(message), None))
            .ok()
    }

    pub async fn stop_job(&self, message: StopJobMessage) -> Option<()> {
        self.outbox
            .send((switchboard_supervisor::Message::StopJob(message), None))
            .ok()
    }

    pub async fn request_status(&self) -> Option<SupervisorStatus> {
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
            if let ResponseMessage::StatusResponse(status) = response {
                Some(status)
            } else {
                tracing::error!("invalid response to StatusRequest: {response:?}");
                None
            }
        })
    }
}

async fn try_close(
    socket: &mut WebSocket,
    supervisor_id: Uuid,
    maybe_cf: Option<CloseFrame<'static>>,
) {
    if let Err(e) = socket.send(WsMessage::Close(maybe_cf)).await {
        tracing::error!("Failed to send close frame to supervisor ({supervisor_id}): {e}.");
    }
    // .send(..::Close(..)) already closes the socket, so no need to call .close()
}

struct SwitchboardConnector {
    supervisor_id: Uuid,
    outbox: UnboundedReceiver<OutboxMessage>,
    event_report_queue: UnboundedSender<SupervisorEvent>,
    outstanding: DashMap<Uuid, oneshot::Sender<ResponseMessage>>,
}
impl SwitchboardConnector {
    async fn run(&mut self, mut socket: WebSocket) {
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
            }
        }
        // next() -> None -indicates-> connection closed
        tracing::error!("connection closed unexpectedly by supervisor");
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
                let _ = self.outstanding.insert(id, notifier);
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
                if let Some((_, notifier)) = self.outstanding.remove(&rm.response_to_request_id) {
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
                            try_close(socket, supervisor_id, None).await;
                            return ControlFlow::Break(());
                        }
                    };
                    // TODO: control flow -> allow errors to break the loop?
                    self.handle_switchboard_message(m)
                }
                WsMessage::Binary(_) => {
                    tracing::error!("Received binary message from supervisor ({supervisor_id})");
                    try_close(socket, supervisor_id, None).await;
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
                try_close(socket, self.supervisor_id, None).await;
                return ControlFlow::Break(());
            }
        }
    }
}
