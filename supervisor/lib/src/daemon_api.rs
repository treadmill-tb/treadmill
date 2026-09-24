use std::net::SocketAddr;
use std::time::Duration;

use aide::axum::ApiRouter;
use aide::axum::routing::{get_with, put_with};
use aide::openapi::{Info, OpenApi};
use anyhow::{Context, Result};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use treadmill_rs::api::supervisor_daemon::{JOB_PATH, JOB_READY_PATH, JobInfo};

use crate::job::JobHandle;

const BIND_ATTEMPTS: usize = 20;
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(100);

pub struct DaemonApi {
    shutdown: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

impl DaemonApi {
    pub async fn serve(addr: SocketAddr, handle: JobHandle) -> Result<Self> {
        let listener = bind(addr).await?;
        let shutdown = CancellationToken::new();
        let app: axum::Router = api_router().with_state(handle).into();
        let task = tokio::spawn(
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned())
                .into_future(),
        );
        Ok(DaemonApi { shutdown, task })
    }

    pub async fn shutdown(self) -> Result<()> {
        self.shutdown.cancel();
        self.task
            .await
            .context("Joining the daemon API task")?
            .context("Serving the daemon API")
    }
}

pub fn openapi_spec() -> OpenApi {
    let mut api = OpenApi {
        info: Info {
            title: "Treadmill Supervisor Daemon API".to_string(),
            version: "0.1.0".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = api_router().finish_api(&mut api);
    api
}

fn api_router() -> ApiRouter<JobHandle> {
    ApiRouter::new()
        .api_route(
            JOB_PATH,
            get_with(job_info, |o| {
                o.id("getJob")
                    .response_with::<404, (), _>(|r| r.description("The job has terminated."))
            }),
        )
        .api_route(
            JOB_READY_PATH,
            put_with(ready, |o| {
                o.id("reportReady")
                    .response_with::<204, (), _>(|r| r.description("The job is ready."))
            }),
        )
}

async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    let mut attempts_left = BIND_ATTEMPTS;
    loop {
        match TcpListener::bind(addr).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempts_left > 1 => {
                attempts_left -= 1;
                tokio::time::sleep(BIND_RETRY_INTERVAL).await;
            }
            res => return res.with_context(|| format!("Binding to {addr:?}")),
        }
    }
}

async fn job_info(State(handle): State<JobHandle>) -> Result<Json<JobInfo>, StatusCode> {
    let facts = handle.facts();
    if facts.phase.terminated() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(JobInfo {
        job_id: facts.job_id,
        api: facts.api.clone(),
    }))
}

async fn ready(State(handle): State<JobHandle>) -> StatusCode {
    handle.daemon_ready();
    StatusCode::NO_CONTENT
}
