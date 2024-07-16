use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

use treadmill_rs::api::supervisor_puppet::{PuppetMsg, SupervisorMsg};
use treadmill_rs::control_socket::Supervisor;

#[derive(Debug, Clone)]
enum ControlSocketTaskCommand {
    Shutdown,
}

pub struct TcpControlSocket<S: Supervisor> {
    job_id: Uuid,
    task_handle: JoinHandle<Result<()>>,
    task_cmd_chan: tokio::sync::mpsc::Sender<ControlSocketTaskCommand>,
    _supervisor: Arc<S>,
}

impl<S: Supervisor> fmt::Debug for TcpControlSocket<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("TcpControlSocket")
            .field("job_id", &self.job_id)
            .finish()
    }
}

impl<S: Supervisor> TcpControlSocket<S> {
    pub async fn new(
        job_id: Uuid,
        bind_addr: std::net::SocketAddr,
        supervisor: Arc<S>,
    ) -> Result<Self> {
        let server_socket: TcpListener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("Binding to TCP socket at {:?}", bind_addr))?;

        info!("Opened control socket TCP listener on {:?}", bind_addr);

        let (task_cmd_chan_tx, mut task_cmd_chan_rx) = tokio::sync::mpsc::channel(1);

        let task_supervisor = supervisor.clone();

        let task_handle = tokio::spawn(async move {
            use futures::SinkExt;
            use tokio_stream::StreamExt;

            let supervisor = task_supervisor;

            let mut shutdown_requested = false;

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
                    Ok((socket, _client_addr)) => socket,
                    Err(ControlSocketTaskCommand::Shutdown) => {
                        shutdown_requested = true;
                        continue;
                    }
                };

                let mut transport = Framed::new(socket, LengthDelimitedCodec::new());

                loop {
                    #[rustfmt::skip]
                    let res = tokio::select! {
                        recv_res = transport.next() => {
                            match recv_res {
                                Some(Ok(bytes)) => Ok(bytes),
                                // TODO: replace with error
				// TODO: what happens on stream close?
                                Some(Err(e)) => panic!("Error occurred while receiving from control socket: {:?}", e),
				None => panic!("Error occurred while receiving from control socket: None"),
                            }
                        }

                        cmd_res = task_cmd_chan_rx.recv() => {
                            error!("Received task command inner!");
                            match cmd_res {
                                Some(cmd) => Err(cmd),
                                None => {
                                    panic!("Control socket command channel TX dropped!");
                                },
                            }
                        }
                    };

                    // Handle either an incoming request or a command.
                    match res {
                        Ok(bytes) => {
                            // Attept to decode the datagram. If this fails,
                            // send a RequestError response containing the error
                            // message. Otherwise, pass the request onto the
                            // handle function:
                            debug!("Trying to decode {:?}", &bytes);
                            let opt_resp = match serde_json::from_slice(&bytes) {
                                Ok(PuppetMsg::Request {
                                    request_id,
                                    request,
                                }) => Some(SupervisorMsg::Response {
                                    request_id,
                                    response: supervisor
                                        .handle_request(request_id, request, job_id)
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
                                use bytes::BufMut;

                                let mut bytes = bytes::BytesMut::new().writer();
                                serde_json::to_writer(&mut bytes, &resp)
                                    .expect("Failed to encode control socket response as JSON");

                                if let Err(e) = transport.send(bytes.into_inner().freeze()).await {
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

        Ok(TcpControlSocket {
            job_id,
            task_handle,
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

        // The remainding cleanup happens when self is dropped.
        Ok(())
    }
}
