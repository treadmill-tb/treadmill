use std::sync::Arc;

use async_trait::async_trait;
use reqwest::RequestBuilder;
use serde_json::json;
use treadmill_rs::{
    api::coord_supervisor::{rest, sse},
    connector::{self, JobError, StartJobRequest, StopJobRequest, Supervisor},
};
use uuid::Uuid;

pub struct HttpConnector<S> {
    http_client: reqwest::Client,
    base_url: String,
    ws_url: String,
    supervisor: Arc<S>,
}

impl<S> HttpConnector<S> {
    fn post(&self, path: String) -> RequestBuilder {
        // TODO: Add authentication, when appropriate, here
        self.http_client.get(format!("{}/{}", self.base_url, path))
    }
}

#[async_trait]
impl<S: Supervisor> connector::SupervisorConnector for HttpConnector<S> {
    async fn run(&self) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::connect_async;

        let (mut write, mut read) = connect_async(&self.ws_url)
            .await
            .expect("connect").0.split();

	loop {
	    if let Some(Ok(message)) = read.next().await {
		match serde_json::from_slice(&message.into_data()) {
		    Ok(sse::SSEMessage::StartJob(sse::StartJobMessage {
			request_id,
			job_id,
			resume_job,
			image_id,
			ssh_keys,
			ssh_rendezvous_servers,
			parameters,
			run_command,
		    })) => {
			if let Err(err) = Supervisor::start_job(
			    &self.supervisor,
			    StartJobRequest {
				request_id,
				job_id,
				resume_job,
				image_id,
				ssh_keys,
				ssh_rendezvous_servers,
				parameters,
				run_command,
			    }).await {
				write.send(serde_json::to_vec(&json!({
				    "success": false,
				    "error": err
				})).expect("serialize error").into()).await.expect("respond ok");
			    } else {
				write.send(serde_json::to_vec(&json!({
				    "success": true,
				})).expect("serialize error").into()).await.expect("respond ok");
			    }
		    },
		    Ok(sse::SSEMessage::StopJob(sse::StopJobMessage { request_id, job_id })) => {
			if let Err(err) = Supervisor::stop_job(&self.supervisor, StopJobRequest { request_id, job_id })
			    .await {
				write.send(serde_json::to_vec(&json!({
				    "success": false,
				    "error": err
				})).expect("serialize error").into()).await.expect("respond ok");
			    } else {
				write.send(serde_json::to_vec(&json!({
				    "success": true,
				})).expect("serialize error").into()).await.expect("respond ok");
			    }
		    }
		    Ok(sse::SSEMessage::RequestStatusUpdate { .. }) => {
			// TODO
		    }
		    _ => {}
		}
	    }
	}
    }

    async fn update_job_state(&self, job_id: Uuid, job_state: rest::JobState) {
        self.post(format!("state/{}", job_id))
            .json(&json!({
            "id": job_id,
            "state": job_state,
            }))
            .send()
            .await
            .expect("request");
    }

    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        self.post(format!("error/{}", job_id))
            .json(&json!({
            "id": job_id,
            "error": error,
            }))
            .send()
            .await
            .expect("request");
    }

    // TODO: we'll likely want to remove this method from here, and instead have
    // supervisors directly interact with log servers to push events. Or have
    // connectors perform these interactions for them...
    async fn send_job_console_log(&self, _job_id: Uuid, _console_bytes: Vec<u8>) {
    }
}

#[cfg(test)]
mod test {
    use async_trait::async_trait;
    use std::sync::Arc;

    use super::*;
    use treadmill_rs::connector::{StartJobRequest, StopJobRequest, SupervisorConnector};

    struct MockSupervisor;

    #[async_trait]
    impl Supervisor for MockSupervisor {
        async fn start_job(_this: &Arc<Self>, _request: StartJobRequest) -> Result<(), JobError> {
            Ok(())
        }

        async fn stop_job(_this: &Arc<Self>, _request: StopJobRequest) -> Result<(), JobError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_update_job_state() {
        let connector = HttpConnector {
            http_client: reqwest::Client::new(),
            base_url: "http://localhost:1112".into(),
            ws_url: "ws://localhost:1112/connector".into(),
            supervisor: Arc::new(MockSupervisor),
        };
        connector
            .update_job_state(
                Uuid::new_v4(),
                rest::JobState::Finished {
                    status_message: None,
                },
            )
            .await;
    }

    #[tokio::test]
    async fn test_report_job_error() {
        let connector = HttpConnector {
            http_client: reqwest::Client::new(),
            base_url: "http://localhost:1112".into(),
            ws_url: "ws://localhost:1112/connector".into(),
            supervisor: Arc::new(MockSupervisor),
        };
        connector
            .report_job_error(
                Uuid::new_v4(),
                JobError {
                    request_id: None,
                    error_kind: connector::JobErrorKind::JobNotFound,
                    description: "Hello".into(),
                },
            )
            .await;
    }
}
