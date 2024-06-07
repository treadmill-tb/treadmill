use anyhow::{Context, Result};
use log::{info, warn};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};
use uuid::Uuid;

use treadmill_rs::api::supervisor_puppet::{PuppetMsg, PuppetReq, SupervisorMsg, SupervisorResp};
use treadmill_rs::control_socket::Supervisor;

#[derive(Debug, Clone)]
enum ControlSocketTaskCommand {
    Shutdown,
}

struct ControlSocketState {
    client: Option<UnixSeqpacket>,
}

pub struct UnixSeqpacketControlSocket<S: Supervisor> {
    _job_id: Uuid,
    task_handle: JoinHandle<Result<()>>,
    task_cmd_chan: tokio::sync::mpsc::Sender<ControlSocketTaskCommand>,
    state: Arc<RwLock<ControlSocketState>>,
    _supervisor: Arc<S>,
}

impl<S: Supervisor> UnixSeqpacketControlSocket<S> {
    async fn handle_request(
        _request_id: u64,
        req: PuppetReq,
        job_id: Uuid,
        supervisor: &S,
        _state: &mut ControlSocketState,
    ) -> SupervisorResp {
        match req {
            PuppetReq::Ping => SupervisorResp::PingResp,

            PuppetReq::SSHKeys => SupervisorResp::SSHKeysResp {
                ssh_keys: supervisor.ssh_keys(job_id).await.unwrap_or_else(|| vec![]),
            },

            PuppetReq::NetworkConfig => {
                if let Some(nc) = supervisor.network_config(job_id).await {
                    SupervisorResp::NetworkConfig(nc)
                } else {
                    SupervisorResp::JobNotFound
                }
            }

            _ => SupervisorResp::UnsupportedRequest,
        }
    }

    pub async fn new_unix_seqpacket(job_id: Uuid, addr: &Path, supervisor: Arc<S>) -> Result<Self> {
        let mut server_socket: UnixSeqpacketListener = UnixSeqpacketListener::bind(addr)
            .with_context(|| format!("Binding to UNIX socket at {:?}", addr))?;

        info!(
            "Opened control socket UNIX SeqPacket listener on {:?}",
            addr
        );

        let state = Arc::new(RwLock::new(ControlSocketState { client: None }));

        let (task_cmd_chan_tx, mut task_cmd_chan_rx) = tokio::sync::mpsc::channel(1);

        let task_state = state.clone();
        let task_supervisor = supervisor.clone();

        let task_handle = tokio::spawn(async move {
            const RECV_RSV: usize = 1 * 1024 * 1024;

            let state = task_state;
            let supervisor = task_supervisor;

            let mut shutdown_requested = false;
            let mut recv_buf = vec![0; RECV_RSV];

            while !shutdown_requested {
                // Accept new connections. We only handle one
                // connection at any point in time:
                #[rustfmt::skip]
                let socket_res = tokio::select! {
                    accept_res = server_socket.accept() => Ok(
                        accept_res
                            .context("Accepting new control socket connection.").unwrap()
                    ),

                    cmd_res = task_cmd_chan_rx.recv() => match cmd_res {
                        Some(cmd) => Err(cmd),
                        None => {
                            panic!("Control socket command channel TX dropped!");
                        },
                    },
                };

                let socket = match socket_res {
                    Ok(socket) => socket,
                    Err(ControlSocketTaskCommand::Shutdown) => {
                        shutdown_requested = true;
                        continue;
                    }
                };

                // Place the socket reference into the shared socket state:
                {
                    let mut sock_state = state.write().await;
                    sock_state.client = Some(socket);
                }

                loop {
                    let res = {
                        // Acquire a read-lock over the shared socket
                        // state. This ensures that we can send messages while
                        // receiving:
                        let sock_state = state.read().await;
                        let socket = sock_state
                            .client
                            .as_ref()
                            .expect("Invariant violated: client socket removed while reading!");

                        // Receive datagram. We impose an upper limit of 1MB on
                        // datagrams, so a single .recv() call should be
                        // sufficient:
                        #[rustfmt::skip]
                        tokio::select! {
                            recv_res = socket.recv(&mut recv_buf) => {
                                match recv_res {
                                    Ok(size) => Ok(size),
                                    // TODO: replace with error
                                    Err(e) => panic!("Error occurred while receiving from control socket: {:?}", e),
                                }
                            }

                            cmd_res = task_cmd_chan_rx.recv() => {
                                match cmd_res {
                                    Some(cmd) => Err(cmd),
                                    None => {
                                        panic!("Control socket command channel TX dropped!");
                                    },
                                }
                            }
                        }
                    };

                    // Handle either an incoming request or a
                    // command. Importantly, we currently don't hold the
                    // state lock.
                    match res {
                        Ok(size) => {
                            // For handling the request, acquire a write lock:
                            let mut sock_state = state.write().await;

                            // Attept to decode the datagram. If this fails, send a
                            // RequestError response containing the error
                            // message. Otherwise, pass the request onto the handle
                            // function:
                            let opt_resp = match serde_json::from_slice(&recv_buf[..size]) {
                                Ok(PuppetMsg::Request {
                                    request_id,
                                    request,
                                }) => Some(SupervisorMsg::Response {
                                    request_id,
                                    response: Self::handle_request(
                                        request_id,
                                        request,
                                        job_id,
                                        &supervisor,
                                        &mut sock_state,
                                    )
                                    .await,
                                }),
                                Ok(PuppetMsg::Event {
                                    puppet_event_id,
                                    event,
                                }) => {
                                    info!(
                                        "Received unhandled puppet event: {}, {:?}",
                                        puppet_event_id, event
                                    );
                                    None
                                }
                                Err(e) => Some(SupervisorMsg::Error {
                                    message: format!("{:?}", e),
                                }),
                            };

                            if let Some(resp) = opt_resp {
                                let socket = sock_state.client.as_ref().expect(
                                    "Invariant violated: client socket removed while reading!",
                                );
                                if let Err(e) =
                                    socket
                                        .send(&serde_json::to_vec(&resp).expect(
                                            "Failed to encode control socket response as JSON",
                                        ))
                                        .await
                                {
                                    match e.kind() {
                                        std::io::ErrorKind::BrokenPipe => {
                                            warn!("Puppet closed connection.");
                                            break;
                                        }
                                        _ => {
                                            warn!("Unknown error while sending answer to control socket request, ignoring: {:?}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Err(ControlSocketTaskCommand::Shutdown) => {
                            shutdown_requested = true;
                            break;
                        }
                    }
                }
            }

            Ok(())
        });

        Ok(UnixSeqpacketControlSocket {
            _job_id: job_id,
            task_handle,
            state,
            task_cmd_chan: task_cmd_chan_tx,
            _supervisor: supervisor,
        })
    }

    pub async fn shutdown(self) -> Result<()> {
        log::info!("Requesting shutdown.");
        // First, request shutdown of the task:
        self.task_cmd_chan
            .send(ControlSocketTaskCommand::Shutdown)
            .await
            .with_context(|| {
                format!("Requesting shutdown of the control socket request handler")
            })?;

        // Then, try to join it:
        self.task_handle.await.unwrap().unwrap();

        // Finally, so some cleanup.
        let mut state = self.state.write().await;

        // Joining the task implicitly drops the `UnixSeqpacketListener` to
        // close the FD, such that we can unmount the parent file
        // systems. However, the client connection may still be open. Shut that
        // down:
        state
            .client
            .take()
            .map(|c| c.shutdown(std::net::Shutdown::Both))
            .transpose()
            .context("Shutting down control socket UNIX SeqPacket connection.")?;

        // The remainding cleanup happens when self is dropped.
        Ok(())
    }
}
