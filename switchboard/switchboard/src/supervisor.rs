use crate::model::job::JobModel;
use crate::model::supervisor::SupervisorModel;
use axum::extract::ws;
use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket};
use axum::Error;
use dashmap::{DashMap, DashSet};
use futures_util::{Stream, StreamExt};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot, watch, Mutex, OwnedMutexGuard};
use treadmill_rs::api::switchboard::JobStatus;
use treadmill_rs::api::switchboard_supervisor::{
    self, Request, ResponseMessage, SupervisorEvent, SupervisorStatus,
};
use treadmill_rs::connector::{StartJobMessage, StopJobMessage};
use uuid::Uuid;

mod auth;
pub mod socket;

#[derive(Debug)]
struct JobInner {
    job_model: JobModel,
    status_watch: watch::Receiver<JobStatus>,
    event_stream: UnboundedReceiver<SupervisorEvent>,
}
impl JobInner {
    pub fn id(&self) -> Uuid {
        self.job_model.id()
    }
    pub fn latest_status(&mut self) -> Result<JobStatus, JobMarketError> {
        if self.event_stream.is_closed() {
            Err(JobMarketError::InvalidJob)
        } else {
            Ok(self.status_watch.borrow_and_update().clone())
        }
    }
}
fn tap_event_stream(
    mut event_stream: EventStream,
) -> (
    watch::Receiver<JobStatus>,
    UnboundedReceiver<SupervisorEvent>,
) {
    let (status_tx, status_rx) = watch::channel(JobStatus::Inactive);
    let (other_tx, other_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(event) = event_stream.next().await {
            match &event {
                SupervisorEvent::UpdateJobState {
                    job_id: _,
                    job_state,
                } => {
                    if let Err(_) = status_tx.send(JobStatus::Active {
                        job_state: job_state.clone(),
                    }) {
                        break;
                    }
                }
                SupervisorEvent::ReportJobError { job_id: _, error } => {
                    if let Err(_) = status_tx.send(JobStatus::Error {
                        job_error: error.clone(),
                    }) {
                        break;
                    }
                }
                SupervisorEvent::SendJobConsoleLog { .. } => {}
            }
            if let Err(_) = other_tx.send(event) {
                break;
            }
        }
    });

    (status_rx, other_rx)
}
#[derive(Debug)]
pub struct ActiveJob(parking_lot::Mutex<JobInner>);
impl ActiveJob {
    pub fn id(&self) -> Uuid {
        self.0.lock().id()
    }
    pub fn latest_status(&self) -> Result<JobStatus, JobMarketError> {
        self.0.lock().latest_status()
    }
}

#[derive(Debug, Error)]
pub enum JobMarketError {
    #[error("no such job")]
    InvalidJob,
}

#[derive(Debug)]
pub struct JobMarket {
    jobs: DashMap<Uuid, Arc<ActiveJob>>,
}
impl JobMarket {
    pub fn new() -> Self {
        Self {
            jobs: DashMap::new(),
        }
    }
    pub fn insert_active(&self, active_job: Arc<ActiveJob>) {
        // if it's being inserted, it's already been inserted into the database, and if the FKEY
        // constraint didn't fail there, then we won't encounter a collision here, either
        let _ = self.jobs.insert(active_job.id(), active_job);
    }
    pub fn job_status(&self, job_id: Uuid) -> Result<JobStatus, JobMarketError> {
        self.jobs
            .get(&job_id)
            .ok_or(JobMarketError::InvalidJob)?
            .latest_status()
    }
}

#[derive(Debug)]
pub struct SupervisorActor {
    // model: SupervisorModel,
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
    // /// No job is currently running on the supervisor.
    // #[error("no job currently running on supervisor")]
    // IdleSupervisor,
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
    pub async fn add_supervisor(self: Arc<Self>, model: &SupervisorModel, socket: WebSocket) {
        let supervisor_id = model.id();
        let (loop_task_jh, outbox, event_receiver) = supervisor_run(supervisor_id, socket).await;
        let actor = SupervisorActor {
            agent: SupervisorAgent {
                outbox,
                event_receiver: Arc::new(Mutex::new(event_receiver)),
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
        job_model: JobModel,
        on_supervisor_id: Uuid,
    ) -> Result<Arc<ActiveJob>, HerdError> {
        let actor = self
            .supervisors
            .get(&on_supervisor_id)
            .ok_or(HerdError::InvalidSupervisor)?;
        let active_job;
        {
            let mut job_lg = actor.job.lock().await;
            if job_lg.is_some() {
                return Err(HerdError::BusySupervisor);
            }
            let event_stream = actor.agent.event_stream().await;
            let (status_watch, dupe_stream) = tap_event_stream(event_stream);
            active_job = Arc::new(ActiveJob(parking_lot::Mutex::new(JobInner {
                job_model,
                status_watch,
                event_stream: dupe_stream,
            })));
            let _ = job_lg.insert(Arc::downgrade(&active_job));
        }
        // if it returns None, then the supervisor connection closed for some reason in the last few
        // milliseconds
        actor
            .agent
            .start_job(job)
            .await
            .ok_or(HerdError::InvalidSupervisor)?;

        Ok(active_job)
    }

    pub async fn status(&self, supervisor_id: Uuid) -> Result<SupervisorStatus, HerdError> {
        let actor = self
            .supervisors
            .get(&supervisor_id)
            .ok_or(HerdError::InvalidSupervisor)?;
        actor
            .agent
            .request_status()
            .await
            .ok_or(HerdError::InvalidSupervisor)
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
pub struct EventStream {
    inner: OwnedMutexGuard<UnboundedReceiver<SupervisorEvent>>,
}
impl Stream for EventStream {
    type Item = SupervisorEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_recv(cx)
    }
}

/// Interacts with a [`SupervisorConnector`]'s [`run`](Supervisor::run) loop via message passing.
#[derive(Debug)]
pub struct SupervisorAgent {
    outbox: UnboundedSender<OutboxMessage>,
    // needs &mut
    event_receiver: Arc<Mutex<UnboundedReceiver<SupervisorEvent>>>,
}
impl SupervisorAgent {
    /// Returns [`None`] if the connection closed.
    pub async fn event_stream(&self) -> EventStream {
        let lg = Mutex::lock_owned(self.event_receiver.clone()).await;
        EventStream { inner: lg }
    }

    /// Returns [`None`] if the connection closed.
    pub async fn start_job(&self, message: StartJobMessage) -> Option<()> {
        self.outbox
            .send((switchboard_supervisor::Message::StartJob(message), None))
            .ok()
    }

    /// Returns [`None`] if the connection closed.
    pub async fn stop_job(&self, message: StopJobMessage) -> Option<()> {
        self.outbox
            .send((switchboard_supervisor::Message::StopJob(message), None))
            .ok()
    }

    /// Currently not implemented on the supervisor end.
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
