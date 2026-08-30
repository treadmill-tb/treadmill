//! NBD-netboot supervisor — **stubbed during the OCI image migration**.
//!
//! The OCI cutover stripped the home-grown TOML image format that this
//! supervisor's job lifecycle was built on. Its own migration — runtime backing
//! chains served over NBD by `qemu-storage-daemon` and the writeable FAT
//! `/boot` — still has to rewrite the boot-archive/TFTP plumbing against the
//! final OCI shapes. Only the QEMU supervisor is migrated so far.
//!
//! Until then this binary keeps its configuration schema and connector wiring,
//! so deployments still parse and the supervisor still registers and reports
//! itself idle; a job dispatched to it fails at the first lifecycle step. The
//! pre-cutover implementation is preserved in git history (commit `705e010`).

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::signal::unix::SignalKind;
use tokio::sync::mpsc;
use tracing::{Level, event};

use treadmill_rs::api::switchboard_supervisor::LogView;
use treadmill_rs::connector::{JobError, JobErrorKind, StartJobMessage, SupervisorConnector};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_supervisor_lib::bootstrap::{self, COORD_MAILBOX_CAPACITY, OnDisconnect};
use treadmill_supervisor_lib::job::{JobBackend, JobRunner, JobRunnerConfig, JobVars, Workload};
use treadmill_supervisor_lib::job_log;
use treadmill_supervisor_lib::launcher::{self, ProcessLauncher};
use treadmill_supervisor_lib::oci_store::{ImageStore, OciStore, OciStoreConfig};
use treadmill_supervisor_lib::publisher::LogPublisherConfig;
use treadmill_supervisor_lib::workdirs::{JobWorkdirs, RetentionConfig};

#[derive(Parser, Debug, Clone)]
pub struct NbdNetbootSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,
}

// The configuration schema is retained verbatim so existing deployment configs
// still parse; the fields are consumed again when the job lifecycle is
// restored.
#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
pub struct NbdNetbootConfig {
    /// QEMU NBD server binary.
    qemu_nbd_binary: PathBuf,

    /// `qemu-img` binary, to work with qcow2 files.
    qemu_img_binary: PathBuf,

    /// `tar` binary, to pack and unpack the boot TFTP archive.
    tar_binary: PathBuf,

    /// Directory to keep state:
    state_dir: PathBuf,

    /// Maximum "working" disk image to be allocated for a job, in bytes.
    working_disk_max_bytes: u64,

    tcp_control_socket_listen_addr: std::net::SocketAddr,
    nbd_server_listen_addr: std::net::SocketAddr,

    /// TFTP boot file system path.
    tftp_boot_dir: PathBuf,

    /// Start the netboot target.
    start_script: PathBuf,

    /// Stop the netboot target.
    stop_script: PathBuf,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
pub struct NbdNetbootSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configurations for individual connector implementations. All are
    /// optional, and not all of them have to be supported:
    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    /// Local OCI store (per-server Zot daemon) the supervisor pulls images from.
    oci_store: OciStoreConfig,

    /// Local tuning of the console capture→publish path. Optional: omitting
    /// the section leaves every field at its default.
    #[serde(default)]
    log_streaming: LogPublisherConfig,

    nbd_netboot: NbdNetbootConfig,
}

/// The NBD-netboot half of the job lifecycle, not yet migrated to OCI images.
///
/// What a job needs before it can boot — the runtime backing chain served over
/// NBD by `qemu-storage-daemon`, the writeable FAT `/boot` shipped over TFTP —
/// still has to be rewritten against the OCI shapes. Until it is, a dispatched
/// job fails at the first step: the runner reports the error and terminates the
/// job, so the switchboard can remove it and place it elsewhere rather than the
/// daemon falling over.
///
/// The image store and launcher seams are held so restoring the lifecycle does
/// not have to re-plumb `main`.
#[allow(dead_code)]
#[derive(Debug)]
struct NbdNetbootBackend {
    image_store: Arc<dyn ImageStore>,
    launcher: Arc<dyn ProcessLauncher>,
    config: NbdNetbootConfig,
}

#[async_trait]
impl JobBackend for NbdNetbootBackend {
    // Uninhabited: no image is ever resolved, which makes the steps that would
    // consume one unreachable rather than unimplemented.
    type Image = Infallible;
    type Allocation = Infallible;

    async fn fetch(&self, _job: &StartJobMessage) -> Result<Infallible, JobError> {
        Err(JobError {
            error_kind: JobErrorKind::InternalError,
            description: "This supervisor cannot run jobs: the nbd-netboot lifecycle awaits \
                          its OCI migration."
                .to_string(),
        })
    }

    async fn allocate(
        &self,
        _job: &StartJobMessage,
        _workdir: &Path,
        image: Infallible,
        _vars: &mut JobVars,
    ) -> Result<Infallible, JobError> {
        match image {}
    }

    async fn launch(
        &self,
        _job: &StartJobMessage,
        _workdir: &Path,
        allocation: Infallible,
        _vars: &JobVars,
    ) -> Result<Workload, JobError> {
        match allocation {}
    }

    /// No views: this backend launches no workload, so a job it runs produces
    /// no console channels.
    fn log_views(&self) -> Vec<LogView> {
        Vec::new()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = NbdNetbootSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file)
        .with_context(|| format!("Reading config file {:?}", args.config_file))?;
    let config: NbdNetbootSupervisorConfig = toml::from_str(&config_str)
        .with_context(|| format!("Parsing config file {:?}", args.config_file))?;

    // The subscriber needs the configured job-log threshold, so it goes up
    // after the config is read; anything failing before this is reported by
    // `main` returning it.
    let job_log = job_log::init_tracing(&config.log_streaming.job_log_level)?;
    event!(Level::INFO, "Treadmill NbdNetboot Supervisor, Hello World!");

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.nbd_netboot.qemu_img_binary.clone(),
    ));

    let workdirs =
        JobWorkdirs::start(&config.nbd_netboot.state_dir, RetentionConfig::default()).await?;

    let backend = Arc::new(NbdNetbootBackend {
        image_store,
        launcher,
        config: config.nbd_netboot.clone(),
    });
    let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

    let connector: Arc<dyn SupervisorConnector> = match config.base.coord_connector {
        SupervisorCoordConnector::WsConnector => {
            let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                "Requested WsConnector, but `ws_connector` config not present."
            ))?;

            Arc::new(treadmill_ws_connector::WsConnector::new(
                config.base.supervisor_id,
                ws_connector_config,
                command_tx,
            ))
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    };

    let runner = Arc::new(JobRunner::new(
        connector.clone(),
        backend,
        JobRunnerConfig {
            supervisor_id: config.base.supervisor_id,
            job_address: config.base.job_address,
            workdirs,
            control_socket_listen_addr: config.nbd_netboot.tcp_control_socket_listen_addr,
            start_script: Some(config.nbd_netboot.start_script.clone()),
            stop_script: Some(config.nbd_netboot.stop_script.clone()),
            log_streaming: config.log_streaming.clone(),
            job_log,
        },
    ));

    bootstrap::serve(
        connector,
        runner,
        command_rx,
        SignalKind::hangup(),
        OnDisconnect::Reconnect,
    )
    .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example is a deployment's starting point, so it has to
    /// parse as the configuration this supervisor actually reads — the point
    /// of keeping the schema while the lifecycle is stubbed.
    #[test]
    fn the_example_config_parses() {
        toml::from_str::<NbdNetbootSupervisorConfig>(include_str!("../config.example.toml"))
            .unwrap();
    }
}
