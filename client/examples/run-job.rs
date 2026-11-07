//! Enqueue a job, wait for it, and open a local port onto its SSH service —
//! the whole arc a tool built on this crate goes through, with no `tml`
//! binary involved.
//!
//! ```text
//! cargo run -p treadmill-client --example run-job -- <IMAGE-SET-UUID>
//! ```
//!
//! It uses whatever profile `tml` would: `$TML_PROFILE`, or `default`. Log in
//! with `tml login` first, or set `TML_TOKEN`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use treadmill_client::diagnostics::Diagnostics;
use treadmill_client::session::{Options, Session};
use treadmill_client::{ssh, wsproxy};
use treadmill_rs::api::switchboard::jobs::RestartPolicy;
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, JobState};
use uuid::Uuid;

/// Everything the library reports, on stderr. A real tool would log these.
struct Stderr;

impl Diagnostics for Stderr {
    fn warn(&self, message: &str) {
        eprintln!("warning: {message}");
    }
    fn detail(&self, message: &str) {
        eprintln!("{message}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let set_id: Uuid = std::env::args()
        .nth(1)
        .context("usage: run-job <IMAGE-SET-UUID>")?
        .parse()
        .context("the image set id is not a UUID")?;

    // The profile on disk, plus the stored session token.
    let mut session = Session::load(&Options::default(), Arc::new(Stderr))?;
    let client = session.authenticated_client()?;
    eprintln!(
        "as {} at {}",
        client.whoami().await?.name,
        session.config.switchboard
    );

    let enqueued = client
        .enqueue_job(&JobRequest {
            init_spec: JobInitSpec::ImageSet {
                set_id,
                generation: None,
            },
            label: Some("run-job example".to_string()),
            owner: None,
            restart_policy: RestartPolicy { max_restarts: 0 },
            parameters: Default::default(),
            host_cel_predicate: "true".to_string(),
            lease_duration: Some(chrono::Duration::minutes(30)),
            lease_expiry_action: None,
            annotations: Default::default(),
        })
        .await
        .context("enqueuing the job")?;
    let job_id = enqueued.job_id;
    eprintln!("job {job_id}");

    // Wait for it to come up. `tml job wait` polls on the same interval.
    let service = loop {
        let job = client.get_job(job_id).await?;
        match job.state {
            JobState::Ready => break ssh::discover_service(&client, job_id).await?,
            JobState::Terminating | JobState::Finalized => {
                bail!("job {job_id} finished before it was ready: {:?}", job.state)
            }
            state => {
                eprintln!("  {state:?}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };

    // A local port onto the job's SSH service, through the gateway. Each
    // accepted connection is bridged in turn; `ssh -p 2222 tml@localhost`
    // reaches the job.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:2222").await?;
    eprintln!("ssh -p 2222 {}@127.0.0.1", session.config.user);
    loop {
        let (stream, _) = listener.accept().await?;
        let (read, write) = stream.into_split();
        wsproxy::run(&mut session, job_id, read, write, &service, None).await?;
    }
}
