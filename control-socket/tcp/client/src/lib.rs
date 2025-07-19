use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bytes::{BufMut, Bytes, BytesMut};
use log::{debug, error, info, warn};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use treadmill_rs::api::supervisor_puppet::{
    PuppetEvent, PuppetMsg, PuppetReq, SupervisorEvent, SupervisorMsg, SupervisorResp,
};

enum TcpControlSocketClientTaskCmd {
    SendMessage(Bytes),
    Shutdown,
}

pub struct TcpControlSocketClient {
    // socket: Arc<
    puppet_event_cnt: Mutex<u64>,
    request_responses: Arc<Mutex<(u64, HashMap<u64, Option<SupervisorResp>>)>>,
    task_cmd_tx: tokio::sync::mpsc::Sender<TcpControlSocketClientTaskCmd>,
    task_notify: Arc<tokio::sync::Notify>,
    task_join_handle: tokio::task::JoinHandle<()>,
    task_supervisor_event_rx:
        tokio::sync::Mutex<tokio::sync::mpsc::Receiver<(u64, SupervisorEvent)>>,
}

impl TcpControlSocketClient {
    pub async fn new(
        addr: std::net::SocketAddr,
        recv_ev_cap: usize,
    ) -> Result<TcpControlSocketClient> {
        let socket = TcpStream::connect(addr)
            .await
            .with_context(|| format!("Opening TCP control socket connection at {:?}", addr,))?;

        let request_responses = Arc::new(Mutex::new((0, HashMap::new())));

        let task_request_responses = request_responses.clone();
        let task_notify = Arc::new(tokio::sync::Notify::new());
        let task_notify_task = task_notify.clone();
        let (task_cmd_tx, task_cmd_rx) = tokio::sync::mpsc::channel(1);
        let (task_supervisor_event_tx, task_supervisor_event_rx) =
            tokio::sync::mpsc::channel(recv_ev_cap);

        let task_join_handle = tokio::spawn(async move {
            Self::task(
                socket,
                task_request_responses,
                task_cmd_rx,
                task_notify_task,
                task_supervisor_event_tx,
            )
            .await
        });

        Ok(TcpControlSocketClient {
            puppet_event_cnt: Mutex::new(0),
            request_responses,
            task_cmd_tx,
            task_notify,
            task_join_handle,
            task_supervisor_event_rx: tokio::sync::Mutex::new(task_supervisor_event_rx),
        })
    }

    pub async fn autodiscover(recv_ev_cap: usize) -> Option<Result<TcpControlSocketClient>> {
        info!("Attempting auto-discovery of TCP control socket endpoint...");

        // If we're running in a QEMU VM, the hypervisor might give us the
        // socket address to the TCP endpoint via a firmware variable. Check if
        // that file exists:
        let qemu_fw_cfg_path = std::path::Path::new(
            "/sys/firmware/qemu_fw_cfg/by_name/opt/org.tockos.treadmill.tcp-ctrl-socket/raw",
        );
        match tokio::fs::read(qemu_fw_cfg_path).await {
            Ok(tcp_sockaddr_bytes) => {
                // TODO -- better function available on nightly:
                // match std::net::SocketAddr::parse_ascii(&tcp_sockaddr_bytes) {
                match std::str::from_utf8(&tcp_sockaddr_bytes)
                    .map(|s| <std::net::SocketAddr as std::str::FromStr>::from_str(s))
                {
                    Ok(Ok(addr)) => {
                        info!("Discovered TCP socket address from qemu_fw_cfg: {:?}", addr);
                        return Some(Self::new(addr, recv_ev_cap).await);
                    }

                    Ok(Err(addr_parse_error)) => {
                        warn!(
                            "Error parsing TCP socket address from qemu_fw_cfg ({}): {:?}, qemu_fw_cfg bytes: {:02x?}",
                            qemu_fw_cfg_path.display(),
                            addr_parse_error,
                            tcp_sockaddr_bytes
                        );
                    }

                    Err(utf8_error) => {
                        warn!(
                            "Error parsing TCP socket address from qemu_fw_cfg ({}) -- not valid UTF-8: {:?}, qemu_fw_cfg bytes: {:02x?}",
                            qemu_fw_cfg_path.display(),
                            utf8_error,
                            tcp_sockaddr_bytes
                        );
                    }
                }
            }

            Err(io_err) => {
                if io_err.kind() == std::io::ErrorKind::NotFound {
                    debug!(
                        "Did not find TCP socket address qemu_fw_cfg in sysfs ({})",
                        qemu_fw_cfg_path.display()
                    );
                } else {
                    warn!(
                        "Failed to read qemu_fw_cfg TCP socket address entry in sysfs ({})",
                        qemu_fw_cfg_path.display()
                    );
                }
            }
        }

        // If we reach this point, we failed TCP control socket endpoint
        // autodiscovery:
        None
    }

    pub async fn shutdown(self) -> Result<()> {
        info!("Requesting supervisor socket client to shut down...");

        // The following expects and unwraps trigger on internal errors that
        // should panic instead of returning an error:
        self.task_cmd_tx
            .send(TcpControlSocketClientTaskCmd::Shutdown)
            .await
            .expect("Supervisor socket client task has quit before receiving shutdown signal!");
        self.task_join_handle.await.unwrap();

        Ok(())
    }

    async fn task(
        socket: TcpStream,
        request_responses: Arc<Mutex<(u64, HashMap<u64, Option<SupervisorResp>>)>>,
        mut cmd_rx: tokio::sync::mpsc::Receiver<TcpControlSocketClientTaskCmd>,
        notify: Arc<tokio::sync::Notify>,
        task_supervisor_event_tx: tokio::sync::mpsc::Sender<(u64, SupervisorEvent)>,
    ) {
        use futures::SinkExt;
        use tokio_stream::StreamExt;

        let mut transport = Framed::new(socket, LengthDelimitedCodec::new());

        loop {
            #[rustfmt::skip]
            tokio::select! {
		cmd_res = cmd_rx.recv() => {
                    match cmd_res {
			None => {
			    panic!("Task command channel TX dropped before shutdown!");
			},

			Some(TcpControlSocketClientTaskCmd::Shutdown) => {
			    debug!("Shutting down supervisor socket client");
			    break;
			},

			Some(TcpControlSocketClientTaskCmd::SendMessage(bytes)) => {
			    match transport.send(bytes).await {
				Ok(()) => (),
				Err(e) => {
				    error!("Error sending message to supervisor: {:?}", e);
				}
			    }
			},
                    }
		}

		recv_res = transport.next() => {
                    let bytes = match recv_res {
			Some(Err(e)) => {
			    error!("Failed to receive supervisor message: {:?}", e);
			    continue;
			}
			Some(Ok(b)) => b,
			None => {
			    // TODO: this is likely end of stream?
			    error!("Failed to receive supervisor message: EOF, shutting down supervisor socket client");
			    break;
			}
                    };

                    match serde_json::from_slice(&bytes) {
			Ok(SupervisorMsg::Response {
			    request_id,
			    response,
			}) => {
			    let resp_map = &mut request_responses.lock().await.1;
			    if let Some(entry) = resp_map.get_mut(&request_id) {
				if entry.is_some() {
				    error!("Received spurious response for request ID {}: {:?}",
					   request_id, response);
				}
				*entry = Some(response);
				notify.notify_waiters();
			    } else {
				error!("Received response for unexpected request ID {}: {:?}",
				       request_id, response);
			    }
			},

			Ok(SupervisorMsg::Event {
			    supervisor_event_id,
			    event,
			}) => {
			    match task_supervisor_event_tx.try_send((supervisor_event_id, event)) {
				Ok(()) => (),
				Err(tokio::sync::mpsc::error::TrySendError::Full((supervisor_event_id, event))) => {
				    warn!("Discarding received supervisor event with id {}, channel full: {:?}",
					  supervisor_event_id, event);
				},
				Err(tokio::sync::mpsc::error::TrySendError::Closed((supervisor_event_id, event))) => {
				    warn!("Discarding received supervisor event with id {}, channel closed: {:?}",
					  supervisor_event_id, event);
				},
			    }
			}

			Ok(SupervisorMsg::Error {
			    message,
			}) => {
			    warn!("Received error message from supervisor: {:?}", message);
			}

			Err(e) => {
			    panic!("Couldn't parse supervisor message: {:?}", e);
			}
                    }
		}
            }
        }
    }

    pub async fn request(&self, req: PuppetReq) -> Result<SupervisorResp> {
        let request_id = {
            // Acquire request ID:
            let mut request_responses_lg = self.request_responses.lock().await;

            let request_id = request_responses_lg.0;

            // Fine to panic here, if we're overflowing this 64-bit integer then
            // something went very wrong:
            request_responses_lg.0 = request_responses_lg
                .0
                .checked_add(1)
                .expect("Request counter overflow!");

            // Insert dummy value, to indicate that we're actually waiting on
            // this request. This helps debug cases where the supervisor sends a
            // response to an invalid request ID or a request that is no longer
            // current:
            assert!(request_responses_lg.1.insert(request_id, None).is_none());

            request_id
        };

        // Ask the async task to send the request:
        let mut bytes = BytesMut::new().writer();
        serde_json::to_writer(
            &mut bytes,
            &PuppetMsg::Request {
                request_id,
                request: req,
            },
        )
        .context("Encoding the control socket request as JSON")?;

        self.task_cmd_tx
            .send(TcpControlSocketClientTaskCmd::SendMessage(
                bytes.into_inner().freeze(),
            ))
            .await
            .context("Failed to send control socket message: supervisor socket client task is no longer alive!")?;

        // Re-acquire the lock:
        let mut request_responses_lg = self.request_responses.lock().await;

        // Now, while we're hold the lock guard, request a notification, but
        // only await it after releasing the lock to avoid a deadlock:
        //
        // Fine to panic here with `unwrap`. We've placed the request into the
        // HashMap above, and it not being present represents an internal
        // consistency issue.
        while request_responses_lg.1.get(&request_id).unwrap().is_none() {
            let fut = self.task_notify.notified();
            std::mem::drop(request_responses_lg);
            fut.await;
            request_responses_lg = self.request_responses.lock().await;
        }

        // We have a response, extract and return it:
        //
        // Fine to panic here, given the above logic: the response needs to be
        // present, and have a non-`None` value, as checked above:
        Ok(request_responses_lg.1.remove(&request_id).unwrap().unwrap())
    }

    pub async fn send_event(&self, ev: PuppetEvent) -> Result<()> {
        let event_id = {
            let mut puppet_event_cnt = self.puppet_event_cnt.lock().await;
            let event_id = *puppet_event_cnt;
            *puppet_event_cnt = puppet_event_cnt
                .checked_add(1)
                .expect("Puppet event ID overflow!");
            event_id
        };

        // Ask the async task to send the event:
        let mut bytes = BytesMut::new().writer();
        serde_json::to_writer(
            &mut bytes,
            &PuppetMsg::Event {
                puppet_event_id: event_id,
                event: ev,
            },
        )
        .context("Encoding the control socket request as JSON")?;

        self.task_cmd_tx
            .send(TcpControlSocketClientTaskCmd::SendMessage(
                bytes.into_inner().freeze(),
            ))
            .await
            .context("Failed to send control socket message: supervisor socket client task is no longer alive!")?;

        Ok(())
    }

    pub async fn listen(&self) -> Result<(u64, SupervisorEvent)> {
        let mut rx_lg = self.task_supervisor_event_rx.try_lock()
	    .context("Acquiring lock for task_supervisor_event_rx channel. Only one `listen`er can be active at a given time")?;

        rx_lg.recv().await.ok_or(anyhow!(
            "Control socket task_supervisor_event channel is closed, likely shutting down."
        ))
    }
}
