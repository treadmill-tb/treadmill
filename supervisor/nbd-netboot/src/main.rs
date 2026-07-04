//! NBD-netboot supervisor — **stubbed during the OCI image migration**.
//!
//! The OCI cutover (`doc/oci-image-migration-plan.md`) stripped the home-grown
//! TOML image format that this supervisor's job lifecycle was built on. Its real
//! migration — runtime backing chains served over NBD by `qemu-storage-daemon`
//! and the writeable FAT `/boot` (D9/D17) — is **Phase 6.6**, which rewrites the
//! boot-archive/TFTP plumbing against the final OCI shapes. Only the QEMU
//! supervisor is migrated in Phase 2.
//!
//! Until then this binary keeps its configuration schema and connector wiring so
//! deployments still parse and the supervisor still registers, but the job
//! lifecycle (`start_job`/`stop_job`) is `todo!()`. The pre-cutover
//! implementation is preserved in git history (commit `705e010`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tracing::{Level, event, info, instrument, warn};

use treadmill_rs::connector;
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_supervisor_lib::launcher::{self, ProcessLauncher};
use treadmill_supervisor_lib::oci_store::{ImageStore, OciStore, OciStoreConfig};

#[derive(Parser, Debug, Clone)]
pub struct NbdNetbootSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,
}

// The configuration schema is retained verbatim so existing deployment configs
// still parse; the fields are consumed again when Phase 6.6 restores the job
// lifecycle.
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

    nbd_netboot: NbdNetbootConfig,
}

// The image store and launcher seams are constructed and held so Phase 6.6 can
// restore the lifecycle without re-plumbing `main`; unused while stubbed.
#[allow(dead_code)]
#[derive(Debug)]
pub struct NbdNetbootSupervisor {
    connector: Arc<dyn connector::SupervisorConnector>,
    image_store: Arc<dyn ImageStore>,
    launcher: Arc<dyn ProcessLauncher>,
    args: NbdNetbootSupervisorArgs,
    config: NbdNetbootSupervisorConfig,
}

impl NbdNetbootSupervisor {
    pub fn new(
        connector: Arc<dyn connector::SupervisorConnector>,
        image_store: Arc<dyn ImageStore>,
        launcher: Arc<dyn ProcessLauncher>,
        args: NbdNetbootSupervisorArgs,
        config: NbdNetbootSupervisorConfig,
    ) -> Self {
        NbdNetbootSupervisor {
            connector,
            image_store,
            launcher,
            args,
            config,
        }
    }
}

#[async_trait]
impl connector::Supervisor for NbdNetbootSupervisor {
    #[instrument(skip(_this, _start_job_req))]
    async fn start_job(
        _this: &Arc<Self>,
        _start_job_req: connector::StartJobMessage,
    ) -> Result<(), connector::JobError> {
        todo!(
            "Phase 6.6: nbd-netboot OCI migration — runtime backing chain over \
             qemu-storage-daemon + writeable FAT /boot (D9/D17)"
        )
    }

    #[instrument(skip(_this))]
    async fn stop_job(
        _this: &Arc<Self>,
        _msg: connector::StopJobMessage,
    ) -> Result<(), connector::JobError> {
        todo!("Phase 6.6: nbd-netboot OCI migration")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    use treadmill_rs::connector::SupervisorConnector;

    tracing_subscriber::fmt::init();
    event!(Level::INFO, "Treadmill NbdNetboot Supervisor, Hello World!");

    let args = NbdNetbootSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: NbdNetbootSupervisorConfig = toml::from_str(&config_str).unwrap();

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.nbd_netboot.qemu_img_binary.clone(),
    ));

    match config.base.coord_connector {
        SupervisorCoordConnector::WsConnector => {
            use tokio::signal::unix::{SignalKind, signal};

            let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                "Requested WsConnector, but `ws_connector` config not present."
            ))?;

            // Create the supervisor and connector with cyclical references
            let mut connector_opt = None;
            let nbd_supervisor = {
                let connector_opt = &mut connector_opt;
                Arc::new_cyclic(move |weak_supervisor| {
                    let connector = Arc::new(treadmill_ws_connector::WsConnector::new(
                        config.base.supervisor_id,
                        ws_connector_config,
                        weak_supervisor.clone(),
                    ));
                    *connector_opt = Some(connector.clone());

                    NbdNetbootSupervisor::new(connector, image_store, launcher, args, config)
                })
            };

            let connector = connector_opt.take().unwrap();

            // === 1) SIGHUP handler => request graceful shutdown
            let mut hup_signal =
                signal(SignalKind::hangup()).expect("Failed to register SIGHUP handler");
            let connector_for_signal = connector.clone();
            tokio::spawn(async move {
                while hup_signal.recv().await.is_some() {
                    tracing::info!("Received SIGHUP => requesting graceful shutdown...");
                    connector_for_signal.request_shutdown();
                }
            });

            // === 2) Repeatedly call `connector.run()`. Once shutdown is
            //         requested, and `run()` returns, we can break.
            loop {
                if let Err(()) = connector.run().await {
                    warn!("Run method exited with error, trying to reconnect in 1 second...");
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                } else {
                    info!("Run method exited, shutting down supervisor...");
                    break;
                }
            }

            // === 3) Clean up any references and exit. ===
            std::mem::drop(nbd_supervisor);

            Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
}
