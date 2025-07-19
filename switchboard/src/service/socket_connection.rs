//! Handling of the actual WebSocket connection that is used between supervisor and switchboard.
//!
//! The key interface is the [`supervisor_run_loop`] function, which spawns a `tokio` task that
//! handles the actual communication logic of the WebSocket, abstracting it into a set of channels.

use axum::extract::ws;
use axum::extract::ws::{CloseFrame, WebSocket};
use dashmap::DashMap;
use futures_util::StreamExt;
use std::ops::ControlFlow;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot};
use treadmill_rs::api::switchboard_supervisor;
use treadmill_rs::api::switchboard_supervisor::{ResponseMessage, SocketConfig, SupervisorEvent};
use uuid::Uuid;

/// Spawns a tokio task that exchanges messages with the supervisor at the other end of the
/// `socket`.
///
/// Returns `(join_handle, outbox_send, event_recv)`.
/// - `join_handle` is a future that completes when the connection with the supervisor closes.
/// - `outbox_send` is used for sending messages, and takes values of the form
/// `(mesg, Option<resp_send>)`, where the optional one-shot response channel can be used to
/// indicate that a response is expected, and pass the response back to the caller.
/// - `event_recv` is used for receiving messages sent unilaterally from the supervisor.
pub fn supervisor_run_loop(
    supervisor_id: Uuid,
    socket: WebSocket,
    config: SocketConfig,
) -> (
    tokio::task::JoinHandle<()>,
    UnboundedSender<OutboxMessage>,
    UnboundedReceiver<SupervisorEvent>,
) {
    let (outbox_tx, outbox_rx) = mpsc::unbounded_channel();
    let (event_report_queue_tx, event_report_queue_rx) = mpsc::unbounded_channel();

    let mut conn = SupervisorConnection {
        supervisor_id,
        outbox: outbox_rx,
        event_report_queue: event_report_queue_tx,
        outstanding_requests: Default::default(),
    };
    let jh = tokio::spawn(async move {
        conn.run(socket, config).await;
        let _ = conn;
    });

    (jh, outbox_tx, event_report_queue_rx)
}

pub type OutboxMessage = (
    switchboard_supervisor::Message,
    Option<oneshot::Sender<ResponseMessage>>,
);

/// Structure used internally to represent the supervisor connection.
struct SupervisorConnection {
    supervisor_id: Uuid,
    outbox: UnboundedReceiver<OutboxMessage>,
    event_report_queue: UnboundedSender<SupervisorEvent>,
    outstanding_requests: DashMap<Uuid, oneshot::Sender<ResponseMessage>>,
}
impl SupervisorConnection {
    async fn try_close(
        &self,
        socket: &mut WebSocket,
        supervisor_id: Uuid,
        maybe_cf: Option<CloseFrame<'static>>,
    ) {
        if let Err(e) = socket.send(ws::Message::Close(maybe_cf)).await {
            tracing::error!("Failed to send close frame to supervisor ({supervisor_id}): {e}.");
        }
        // .send(..::Close(..)) already closes the socket, so no need to call .close()
    }

    /// Primary run-loop. This owns the actual WebSocket connection. When this exits, the connection
    /// is considered closed.
    async fn run(&mut self, mut socket: WebSocket, config: SocketConfig) {
        // TODO: need more robust method of error handling
        let supervisor_id = self.supervisor_id;

        // Keepalive strategy
        let ping_interval =
            config.keepalive.ping_interval.to_std().expect(
                "Configured WebSocket PING interval is out of range for core's Duration type",
            );
        let keepalive = config.keepalive.keepalive.to_std().expect(
            "Configured WebSocket PONG keepalive period is out of range for core's Duration type",
        );
        let mut interval = tokio::time::interval(ping_interval);
        let mut last_received_pong = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tracing::trace!("Sending PING to supervisor ({supervisor_id})");
                    // let ping_no_as_vec = ping_no.to_le_bytes().to_vec();
                    let elapsed = last_received_pong.elapsed();
                    if elapsed > keepalive {
                        tracing::error!("Haven't received a PONG in {elapsed:?}, exiting socket control loop");
                        return
                    }
                    if let Err(e) = socket.send(ws::Message::Ping(vec![])).await {
                        tracing::error!("Failed to send PING to supervisor ({supervisor_id}): {e}.");
                        return;
                    }
                }
                out = self.outbox.recv() => {
                    if let Some(outbox_message) = out {
                        if self.handle_outgoing_message(&mut socket, outbox_message).await.is_break() {
                            return
                        }
                    }
                }
                r = socket.next() => {
                    if let Some(r) = r {
                        if let Ok(ws::Message::Pong(v)) = r {
                            tracing::trace!("Received PONG from supervisor ({supervisor_id})");
                            if !v.is_empty() {
                                tracing::error!("Received PONG of nonzero size (size {})", v.len());
                                return
                            }
                            last_received_pong = tokio::time::Instant::now();
                            // if let Ok(recvd_ping_no) = v.try_into().map(u64::from_le_bytes) {
                            //     // if ping_no == recvd_ping_no {
                            //     //     ping_no = ping_no.wrapping_add(1);
                            //     // } else {
                            // } else {
                            //     tracing::error!("Unexpected PONG size: {}", v.len());
                            //     return
                            // }
                        } else {
                            tracing::debug!("Received message from supervisor ({supervisor_id}): {r:?}.");
                            if self.handle_incoming_message(&mut socket, supervisor_id, r).await.is_break() {
                                return
                            }
                        }
                    } else {
                        // connection closed without close frame
                        tracing::debug!("Supervisor ({supervisor_id}) closed connection without close frame");
                        break
                    }
                }
            }
        }
        // next() -> None - indicates -> connection closed
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
        let m_as_string = match serde_json::to_string(&m) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to serialize outgoing message: {e}");
                return ControlFlow::Break(());
            }
        };
        if let Err(e) = socket.send(ws::Message::Text(m_as_string)).await {
            tracing::error!("failed to send message over websocket: {e}");
            return ControlFlow::Break(());
        } else {
            tracing::debug!("Sent message to ({}): {m:?}", self.supervisor_id);
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
                    if notifier.send(rm.message).is_err() {
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
        r: Result<ws::Message, axum::Error>,
    ) -> ControlFlow<(), ()> {
        match r {
            Ok(ws_message) => match ws_message {
                ws::Message::Text(s) => {
                    let m: switchboard_supervisor::Message = match serde_json::from_str(&s) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!(
                                "Error deserializing message ({s}) from supervisor ({supervisor_id}): {e}"
                            );
                            self.try_close(socket, supervisor_id, None).await;
                            return ControlFlow::Break(());
                        }
                    };
                    // TODO: control flow -> allow errors to break the loop?
                    self.handle_switchboard_message(m)
                }
                ws::Message::Binary(_) => {
                    tracing::error!("Received binary message from supervisor ({supervisor_id})");
                    self.try_close(socket, supervisor_id, None).await;
                    ControlFlow::Break(())
                }
                ws::Message::Ping(_) => {
                    tracing::trace!("websocket PING'd");
                    ControlFlow::Continue(())
                }
                ws::Message::Pong(_) => {
                    tracing::trace!("websocket PONG'd");
                    ControlFlow::Continue(())
                }
                ws::Message::Close(maybe_cf) => {
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
                    ControlFlow::Break(())
                }
            },
            Err(e) => {
                tracing::error!("Error receiving from socket: {e}, closing.");
                self.try_close(socket, self.supervisor_id, None).await;
                ControlFlow::Break(())
            }
        }
    }
}
