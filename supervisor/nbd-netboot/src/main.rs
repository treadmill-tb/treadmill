use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use clap::Parser;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_serial::SerialPortBuilderExt;
use tokio_util::sync::CancellationToken;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageLocation, ImageSpecification, LogChannel, LogFormat, LogRender, LogView,
};
use treadmill_rs::connector::{JobError, JobErrorKind, StartJobMessage, SupervisorConnector};
use treadmill_rs::image::Digest;
use treadmill_rs::image::annotations::Role;
use treadmill_rs::image::blockdev::BackingChain;
use treadmill_rs::image::media_types;
use treadmill_rs::image::parse::{self, ImageLayer, TreadmillImage};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_supervisor_lib::bootstrap::{self, COORD_MAILBOX_CAPACITY, OnDisconnect, StopSignal};
use treadmill_supervisor_lib::capture::SerialConsole;
use treadmill_supervisor_lib::job::{JobBackend, JobRunner, JobRunnerConfig, JobVars, Workload};
use treadmill_supervisor_lib::job_log::{self, JobLogRegistry, channel_reader};
use treadmill_supervisor_lib::launcher::{
    self, BoxedAsyncRead, ProcessLauncher, StdioMode, WorkloadProcess,
};
use treadmill_supervisor_lib::leases;
use treadmill_supervisor_lib::oci_store::{ImageStore, Location, OciStore, OciStoreConfig};
use treadmill_supervisor_lib::publisher::LogPublisherConfig;
use treadmill_supervisor_lib::workdirs::{AllocationRecord, JobWorkdirs, RetentionConfig};

const ROOT_EXPORT: &str = "root";
const BOOT_EXPORT: &str = "boot";
const BOOT_NODE_PREFIX: &str = "tml-boot";

const ROOT_OVERLAY_FILE: &str = "root.qcow2";
const BOOT_OVERLAY_FILE: &str = "boot.qcow2";

const STORAGE_DAEMON_STDOUT: LogChannel = LogChannel::from_static("storage-daemon-stdout");
const STORAGE_DAEMON_STDERR: LogChannel = LogChannel::from_static("storage-daemon-stderr");
const TFTP: LogChannel = LogChannel::from_static("nbdfatftpd");

const NBD_READY_TIMEOUT: Duration = Duration::from_secs(10);
const NBD_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

const TFTP_RESTART_DELAY_MIN: Duration = Duration::from_secs(1);
const TFTP_RESTART_DELAY_MAX: Duration = Duration::from_secs(30);
const TFTP_STABLE_RUNTIME: Duration = Duration::from_secs(60);
const TFTP_OUTPUT_CAPACITY: usize = 64;

const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug, Clone)]
pub struct NbdNetbootSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,

    #[command(flatten)]
    local_job: Option<treadmill_local_connector::LocalJobArgs>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SerialConsoleConfig {
    device: PathBuf,
    baud_rate: u32,
}

fn default_qemu_storage_daemon_binary() -> PathBuf {
    PathBuf::from("qemu-storage-daemon")
}

fn default_qemu_img_binary() -> PathBuf {
    PathBuf::from("qemu-img")
}

fn default_nbdfatftpd_binary() -> PathBuf {
    PathBuf::from("nbdfatftpd")
}

#[derive(Deserialize, Debug, Clone)]
pub struct NbdNetbootConfig {
    #[serde(default = "default_qemu_storage_daemon_binary")]
    qemu_storage_daemon_binary: PathBuf,

    #[serde(default = "default_qemu_img_binary")]
    qemu_img_binary: PathBuf,

    #[serde(default = "default_nbdfatftpd_binary")]
    nbdfatftpd_binary: PathBuf,

    state_dir: PathBuf,

    working_disk_max_bytes: u64,

    daemon_api_listen_addr: SocketAddr,
    nbd_server_listen_addr: SocketAddr,
    tftp_listen_addr: SocketAddr,

    serial_console: Option<SerialConsoleConfig>,

    #[serde(default)]
    job_retention: RetentionConfig,

    start_script: Option<PathBuf>,
    stop_script: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NbdNetbootSupervisorConfig {
    base: SupervisorBaseConfig,

    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    oci_store: OciStoreConfig,

    #[serde(default)]
    log_streaming: LogPublisherConfig,

    nbd_netboot: NbdNetbootConfig,
}

#[derive(Debug)]
pub struct NetbootImage {
    image: TreadmillImage,
    boot: ImageLayer,
}

pub struct Servers {
    storage_daemon: Box<dyn WorkloadProcess>,
    tftp: TftpServer,
}

impl std::fmt::Debug for Servers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Servers")
    }
}

#[derive(Debug)]
pub struct NbdNetbootBackend {
    image_store: Arc<dyn ImageStore>,
    launcher: Arc<dyn ProcessLauncher>,
    config: NbdNetbootConfig,
}

impl NbdNetbootBackend {
    pub fn new(
        image_store: Arc<dyn ImageStore>,
        launcher: Arc<dyn ProcessLauncher>,
        config: NbdNetbootConfig,
    ) -> Self {
        NbdNetbootBackend {
            image_store,
            launcher,
            config,
        }
    }

    fn nbd_connect_addr(&self) -> SocketAddr {
        let listen = self.config.nbd_server_listen_addr;
        let ip = match listen.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        SocketAddr::new(ip, listen.port())
    }

    fn storage_daemon_args(&self, root: &BackingChain, boot: &BackingChain) -> Vec<String> {
        let listen = self.config.nbd_server_listen_addr;
        let mut args = Vec::new();
        for node in root.blockdev_args().into_iter().chain(boot.blockdev_args()) {
            args.push("--blockdev".to_string());
            args.push(node);
        }
        args.push("--nbd-server".to_string());
        args.push(format!(
            "addr.type=inet,addr.host={},addr.port={},max-connections=0",
            listen.ip(),
            listen.port(),
        ));
        for (export, node) in [
            (ROOT_EXPORT, root.top_node()),
            (BOOT_EXPORT, boot.top_node()),
        ] {
            args.push("--export".to_string());
            args.push(format!(
                "type=nbd,id={export},node-name={node},name={export},writable=on"
            ));
        }
        args
    }

    fn tftp_args(&self) -> Vec<String> {
        vec![
            "-l".to_string(),
            self.config.tftp_listen_addr.to_string(),
            "-n".to_string(),
            format!("nbd://{}/{BOOT_EXPORT}", self.nbd_connect_addr()),
        ]
    }

    async fn create_overlay(&self, path: &Path, virtual_size_bytes: u64) -> Result<(), JobError> {
        event!(
            Level::DEBUG,
            ?path,
            virtual_size_bytes,
            "Creating overlay disk"
        );
        self.launcher
            .create_overlay_no_backing(path, virtual_size_bytes)
            .await
            .map_err(|e| JobError {
                error_kind: JobErrorKind::InternalError,
                description: format!("Failed to allocate {}: {e:#}", path.display()),
            })
    }

    async fn resolve_image(
        &self,
        job_id: Uuid,
        manifest_digest: &Digest,
        locations: &[ImageLocation],
    ) -> Result<NetbootImage, JobError> {
        let locations = locations
            .iter()
            .cloned()
            .map(|loc| Location::new(loc.registry, loc.repository))
            .collect::<Vec<_>>();

        event!(
            Level::TRACE,
            %manifest_digest,
            ?locations,
            "Ensuring image present in the local OCI store",
        );

        self.image_store
            .ensure_present(manifest_digest, &locations)
            .await
            .map_err(|e| JobError {
                error_kind: JobErrorKind::InternalError,
                description: format!("Failed to fetch image {manifest_digest}: {e:#}"),
            })?;

        if let Err(e) = self
            .image_store
            .pin(manifest_digest, &job_id.to_string())
            .await
        {
            event!(
                Level::WARN,
                error = ?e,
                %manifest_digest,
                "Failed to take an in-use lease on the image; it is unprotected against \
                 the local store's garbage collector",
            );
        }

        let manifest = self
            .image_store
            .manifest(manifest_digest)
            .await
            .map_err(|e| JobError {
                error_kind: JobErrorKind::InternalError,
                description: format!("Cannot retrieve image manifest of {manifest_digest}: {e:#}",),
            })?;

        let image = parse::parse_image(&manifest).map_err(|e| JobError {
            error_kind: JobErrorKind::ImageInvalid,
            description: format!("Image {manifest_digest} is not a valid Treadmill image: {e}"),
        })?;

        let boot = boot_layer(&image).map_err(|e| JobError {
            error_kind: JobErrorKind::ImageInvalid,
            description: format!("Image {manifest_digest} cannot netboot: {e}"),
        })?;

        Ok(NetbootImage { image, boot })
    }

    fn chains(
        &self,
        image: &NetbootImage,
        root_overlay: PathBuf,
        boot_overlay: PathBuf,
    ) -> Result<(BackingChain, BackingChain), JobError> {
        let (chain, head_virtual_size) = image.image.backing_chain().map_err(|e| JobError {
            error_kind: JobErrorKind::ImageInvalid,
            description: format!("Invalid backing chain: {e}"),
        })?;

        if head_virtual_size > self.config.working_disk_max_bytes {
            return Err(JobError {
                error_kind: JobErrorKind::ImageInvalid,
                description: format!(
                    "Image head virtual size ({} byte) exceeds the working-disk \
                     maximum ({} byte)",
                    head_virtual_size, self.config.working_disk_max_bytes,
                ),
            });
        }

        let lowers = chain
            .into_iter()
            .map(|layer| self.image_store.blob_path(&layer.digest))
            .collect();

        Ok((
            BackingChain::new(lowers, root_overlay),
            BackingChain::with_prefix(
                BOOT_NODE_PREFIX,
                vec![self.image_store.blob_path(&image.boot.digest)],
                boot_overlay,
            ),
        ))
    }

    fn seed_vars(&self, vars: &mut JobVars) {
        vars.insert(
            "daemon_api_listen_addr".to_string(),
            self.config.daemon_api_listen_addr.to_string(),
        );
        vars.insert(
            "nbd_server_listen_addr".to_string(),
            self.config.nbd_server_listen_addr.to_string(),
        );
        vars.insert(
            "tftp_listen_addr".to_string(),
            self.config.tftp_listen_addr.to_string(),
        );
    }

    async fn start_servers(
        &self,
        job: &StartJobMessage,
        root: &BackingChain,
        boot: &BackingChain,
    ) -> Result<Servers, JobError> {
        let stdio = if job.log_streaming.is_some() {
            StdioMode::Capture
        } else {
            StdioMode::Inherit
        };

        let storage_daemon = self
            .start_storage_daemon(self.storage_daemon_args(root, boot), stdio)
            .await?;

        let tftp = TftpServer::start(
            self.launcher.clone(),
            self.config.nbdfatftpd_binary.clone(),
            self.tftp_args(),
            stdio,
        );

        Ok(Servers {
            storage_daemon,
            tftp,
        })
    }

    async fn start_storage_daemon(
        &self,
        args: Vec<String>,
        stdio: StdioMode,
    ) -> Result<Box<dyn WorkloadProcess>, JobError> {
        event!(
            Level::INFO,
            binary = ?self.config.qemu_storage_daemon_binary,
            ?args,
            "Launching qemu-storage-daemon",
        );
        let mut process = self
            .launcher
            .spawn(&self.config.qemu_storage_daemon_binary, &args, None, stdio)
            .await
            .map_err(|e| JobError {
                error_kind: JobErrorKind::InternalError,
                description: format!("Failed to launch qemu-storage-daemon: {e:#}"),
            })?;

        let addr = self.nbd_connect_addr();
        let ready = async {
            let deadline = Instant::now() + NBD_READY_TIMEOUT;
            while tokio::net::TcpStream::connect(addr).await.is_err() {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "qemu-storage-daemon did not accept connections on {addr} within {NBD_READY_TIMEOUT:?}"
                    ));
                }
                tokio::time::sleep(NBD_READY_POLL_INTERVAL).await;
            }
            Ok(())
        };
        let outcome = tokio::select! {
            ready = ready => ready,
            exit = process.wait() => Err(match exit {
                Ok(status) => format!("qemu-storage-daemon exited during startup with {status}"),
                Err(e) => format!("Failed to wait on qemu-storage-daemon: {e}"),
            }),
        };
        match outcome {
            Ok(()) => Ok(process),
            Err(mut description) => {
                terminate(&mut *process).await;
                if let Some(mut stderr) = process.take_stderr() {
                    let mut output = String::new();
                    let _ = stderr.read_to_string(&mut output).await;
                    if !output.trim().is_empty() {
                        description.push_str(": ");
                        description.push_str(output.trim());
                    }
                }
                Err(JobError {
                    error_kind: JobErrorKind::InternalError,
                    description,
                })
            }
        }
    }

    fn open_serial_console(&self) -> Option<SerialConsole> {
        let config = self.config.serial_console.as_ref()?;
        match tokio_serial::new(config.device.to_string_lossy(), config.baud_rate)
            .open_native_async()
        {
            Ok(stream) => Some(SerialConsole::Stream(Box::new(stream))),
            Err(e) => {
                event!(
                    Level::WARN,
                    device = ?config.device,
                    error = ?e,
                    "Failed to open the serial console; this job ships no serial channel",
                );
                None
            }
        }
    }
}

fn boot_layer(image: &TreadmillImage) -> Result<ImageLayer, String> {
    let mut boots = image
        .layers
        .iter()
        .filter(|layer| layer.role == Some(Role::Boot));
    let boot = boots
        .next()
        .ok_or_else(|| "image has no role=boot layer".to_string())?;
    if boots.next().is_some() {
        return Err("image has more than one role=boot layer".to_string());
    }
    if boot.media_type != media_types::DISK_QCOW2 {
        return Err(format!(
            "boot layer {} has media type {}, expected {}",
            boot.digest,
            boot.media_type,
            media_types::DISK_QCOW2,
        ));
    }
    if boot.virtual_size.is_none() {
        return Err(format!("boot layer {} has no virtual size", boot.digest));
    }
    Ok(boot.clone())
}

#[async_trait]
impl JobBackend for NbdNetbootBackend {
    type Image = NetbootImage;
    type Allocation = Servers;

    async fn fetch(&self, job: &StartJobMessage) -> Result<NetbootImage, JobError> {
        let (manifest_digest, locations) = image_reference(job)?;
        self.resolve_image(job.job_id, &manifest_digest, &locations)
            .await
    }

    #[instrument(skip(self, job, image, vars), err(Debug, level = Level::WARN))]
    async fn allocate(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        image: NetbootImage,
        vars: &mut JobVars,
    ) -> Result<Servers, JobError> {
        let root_overlay = workdir.join(ROOT_OVERLAY_FILE);
        let boot_overlay = workdir.join(BOOT_OVERLAY_FILE);

        let (root, boot) = self.chains(&image, root_overlay.clone(), boot_overlay.clone())?;

        self.create_overlay(&root_overlay, self.config.working_disk_max_bytes)
            .await?;
        self.create_overlay(&boot_overlay, image.boot.virtual_size.unwrap_or_default())
            .await?;

        let (manifest_digest, locations) = image_reference(job)?;
        AllocationRecord::new(
            manifest_digest,
            locations,
            [
                (ROOT_EXPORT.to_string(), ROOT_OVERLAY_FILE.to_string()),
                (BOOT_EXPORT.to_string(), BOOT_OVERLAY_FILE.to_string()),
            ],
        )
        .write(workdir)
        .await
        .map_err(|e| JobError {
            error_kind: JobErrorKind::InternalError,
            description: format!("Failed to record the job's allocation: {e:#}"),
        })?;

        self.seed_vars(vars);

        self.start_servers(job, &root, &boot).await
    }

    #[instrument(skip(self, job, vars), err(Debug, level = Level::WARN))]
    async fn adopt(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        vars: &mut JobVars,
    ) -> Result<Servers, JobError> {
        let record = AllocationRecord::read(workdir)
            .await
            .map_err(cannot_resume)?;
        let root_overlay = record
            .overlay(workdir, ROOT_EXPORT)
            .await
            .map_err(cannot_resume)?;
        let boot_overlay = record
            .overlay(workdir, BOOT_EXPORT)
            .await
            .map_err(cannot_resume)?;

        event!(
            Level::INFO,
            ?root_overlay,
            ?boot_overlay,
            manifest_digest = %record.manifest_digest,
            "Adopting the working disks of a retired job"
        );

        let image = self
            .resolve_image(job.job_id, &record.manifest_digest, &record.locations)
            .await?;
        let (root, boot) = self.chains(&image, root_overlay, boot_overlay)?;

        self.seed_vars(vars);

        self.start_servers(job, &root, &boot).await
    }

    async fn launch(
        &self,
        _job: &StartJobMessage,
        _workdir: &Path,
        servers: Servers,
        _vars: &JobVars,
    ) -> Result<Workload, JobError> {
        let Servers {
            mut storage_daemon,
            mut tftp,
        } = servers;

        let channels = [
            (STORAGE_DAEMON_STDOUT, storage_daemon.take_stdout()),
            (STORAGE_DAEMON_STDERR, storage_daemon.take_stderr()),
            (TFTP, tftp.output.take()),
        ]
        .into_iter()
        .filter_map(|(channel, reader)| reader.map(|reader| (channel, reader)))
        .collect();

        Ok(Workload {
            process: Box::new(NetbootWorkload {
                storage_daemon,
                tftp,
            }),
            serial: self.open_serial_console(),
            channels,
        })
    }

    fn log_views(&self) -> Vec<LogView> {
        netboot_log_views()
    }
}

fn image_reference(job: &StartJobMessage) -> Result<(Digest, Vec<ImageLocation>), JobError> {
    match &job.image_spec {
        ImageSpecification::Image {
            manifest_digest,
            locations,
        } => Ok((*manifest_digest, locations.clone())),

        unsupported_image_spec => Err(JobError {
            error_kind: JobErrorKind::ImageNotCompatible,
            description: format!("Unsupported image specification: {unsupported_image_spec:?}",),
        }),
    }
}

fn cannot_resume(e: anyhow::Error) -> JobError {
    JobError {
        error_kind: JobErrorKind::CannotResume,
        description: format!("{e:#}"),
    }
}

fn netboot_log_views() -> Vec<LogView> {
    vec![
        LogView {
            id: "serial".to_string(),
            label: "Serial console".to_string(),
            render: LogRender::Terminal,
            format: LogFormat::Raw,
            channels: vec![LogChannel::SERIAL],
            order: 10,
            default: true,
            input: true,
        },
        LogView {
            id: "storage-daemon".to_string(),
            label: "Storage daemon".to_string(),
            render: LogRender::Text,
            format: LogFormat::Raw,
            channels: vec![STORAGE_DAEMON_STDOUT, STORAGE_DAEMON_STDERR],
            order: 20,
            default: false,
            input: false,
        },
        LogView {
            id: "tftp".to_string(),
            label: "TFTP server".to_string(),
            render: LogRender::Text,
            format: LogFormat::Raw,
            channels: vec![TFTP],
            order: 21,
            default: false,
            input: false,
        },
    ]
}

async fn terminate(process: &mut dyn WorkloadProcess) {
    if let Err(e) = process.interrupt().await {
        event!(Level::WARN, error = ?e, "Failed to interrupt the process");
    }
    if tokio::time::timeout(GRACEFUL_EXIT_TIMEOUT, process.wait())
        .await
        .is_ok()
    {
        return;
    }
    event!(
        Level::WARN,
        "Process did not exit within {GRACEFUL_EXIT_TIMEOUT:?} of SIGINT, killing it",
    );
    if let Err(e) = process.kill().await {
        event!(Level::WARN, error = ?e, "Failed to kill the process");
    }
}

struct NetbootWorkload {
    storage_daemon: Box<dyn WorkloadProcess>,
    tftp: TftpServer,
}

#[async_trait]
impl WorkloadProcess for NetbootWorkload {
    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.storage_daemon.wait().await
    }

    async fn kill(&mut self) -> std::io::Result<()> {
        self.tftp.stop().await;
        terminate(&mut *self.storage_daemon).await;
        Ok(())
    }

    async fn interrupt(&mut self) -> std::io::Result<()> {
        self.storage_daemon.interrupt().await
    }
}

struct TftpServer {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    output: Option<BoxedAsyncRead>,
}

impl TftpServer {
    fn start(
        launcher: Arc<dyn ProcessLauncher>,
        binary: PathBuf,
        args: Vec<String>,
        stdio: StdioMode,
    ) -> Self {
        let cancel = CancellationToken::new();
        let (output_tx, output_rx) = mpsc::channel(TFTP_OUTPUT_CAPACITY);
        let output = (stdio == StdioMode::Capture).then(|| channel_reader(output_rx));
        let task = tokio::spawn(run_tftp(
            launcher,
            binary,
            args,
            stdio,
            output_tx,
            cancel.clone(),
        ));
        TftpServer {
            cancel,
            task,
            output,
        }
    }

    async fn stop(&mut self) {
        self.cancel.cancel();
        let _ = (&mut self.task).await;
    }
}

impl Drop for TftpServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn run_tftp(
    launcher: Arc<dyn ProcessLauncher>,
    binary: PathBuf,
    args: Vec<String>,
    stdio: StdioMode,
    output: mpsc::Sender<Bytes>,
    cancel: CancellationToken,
) {
    let mut delay = TFTP_RESTART_DELAY_MIN;
    loop {
        let started = Instant::now();
        event!(Level::INFO, ?binary, ?args, "Launching nbdfatftpd");
        let spawned = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            spawned = launcher.spawn(&binary, &args, None, stdio) => spawned,
        };
        match spawned {
            Err(e) => event!(Level::WARN, error = ?e, "Failed to launch nbdfatftpd"),
            Ok(mut process) => {
                for reader in [process.take_stdout(), process.take_stderr()]
                    .into_iter()
                    .flatten()
                {
                    tokio::spawn(forward(reader, output.clone()));
                }
                let exit = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        terminate(&mut *process).await;
                        return;
                    }
                    exit = process.wait() => exit,
                };
                match exit {
                    Ok(status) => event!(Level::WARN, %status, "nbdfatftpd exited"),
                    Err(e) => event!(Level::WARN, error = ?e, "Failed to wait on nbdfatftpd"),
                }
            }
        }

        if started.elapsed() >= TFTP_STABLE_RUNTIME {
            delay = TFTP_RESTART_DELAY_MIN;
        }
        event!(Level::INFO, ?delay, "Restarting nbdfatftpd");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(TFTP_RESTART_DELAY_MAX);
    }
}

async fn forward(mut reader: BoxedAsyncRead, output: mpsc::Sender<Bytes>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if output
                    .send(Bytes::copy_from_slice(&buf[..n]))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

impl NbdNetbootSupervisorConfig {
    fn job_runner(&self, workdirs: Arc<JobWorkdirs>, job_log: JobLogRegistry) -> JobRunnerConfig {
        JobRunnerConfig {
            supervisor_id: self.base.supervisor_id,
            job_address: self.base.job_address,
            workdirs,
            daemon_api_listen_addr: self.nbd_netboot.daemon_api_listen_addr,
            job_api_url: self.base.job_api_url.clone(),
            start_script: self.nbd_netboot.start_script.clone(),
            stop_script: self.nbd_netboot.stop_script.clone(),
            log_streaming: self.log_streaming.clone(),
            job_log,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = NbdNetbootSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file)
        .with_context(|| format!("Reading config file {:?}", args.config_file))?;
    let config: NbdNetbootSupervisorConfig = toml::from_str(&config_str)
        .with_context(|| format!("Parsing config file {:?}", args.config_file))?;

    let job_log = job_log::init_tracing(&config.log_streaming.job_log_level)?;
    event!(Level::INFO, "Treadmill NbdNetboot Supervisor, Hello World!");

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.nbd_netboot.qemu_img_binary.clone(),
    ));

    let workdirs = JobWorkdirs::start(
        &config.nbd_netboot.state_dir,
        config.nbd_netboot.job_retention.clone(),
    )
    .await?;

    leases::spawn_reaper(
        workdirs.clone(),
        image_store.clone(),
        config.nbd_netboot.job_retention.sweep_interval,
    );

    let backend = Arc::new(NbdNetbootBackend::new(
        image_store,
        launcher,
        config.nbd_netboot.clone(),
    ));
    let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

    let (connector, stop_signal, on_disconnect): (Arc<dyn SupervisorConnector>, _, _) =
        match config.base.coord_connector {
            SupervisorCoordConnector::WsConnector => {
                let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                    "Requested WsConnector, but `ws_connector` config not present."
                ))?;

                (
                    Arc::new(treadmill_ws_connector::WsConnector::new(
                        config.base.supervisor_id,
                        ws_connector_config,
                        command_tx,
                    )),
                    StopSignal::AfterJob,
                    OnDisconnect::Reconnect,
                )
            }
            SupervisorCoordConnector::Local => {
                let local_job = args.local_job.clone().unwrap_or_default();
                if local_job.manifest_digest.is_none() || local_job.repository.is_none() {
                    bail!(
                        "The `local` connector requires a job on the command line: both \
                         --manifest-digest and --repository."
                    );
                }

                (
                    Arc::new(treadmill_local_connector::LocalConnector::new(
                        config.oci_store.registry.clone(),
                        local_job,
                        command_tx,
                    )),
                    StopSignal::StopJob,
                    OnDisconnect::Exit,
                )
            }
            unsupported_connector => {
                bail!("Unsupported coord connector: {:?}", unsupported_connector);
            }
        };

    let runner = Arc::new(JobRunner::new(
        connector.clone(),
        backend,
        config.job_runner(workdirs, job_log),
    ));

    bootstrap::serve(connector, runner, command_rx, stop_signal, on_disconnect).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use oci_spec::image::ImageManifest;
    use tempfile::TempDir;
    use tokio::sync::Notify;
    use uuid::Uuid;

    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, LogStreamingDispatch, RestartPolicy,
    };
    use treadmill_rs::image::Digest;
    use treadmill_rs::image::assemble;
    use treadmill_rs::util::Secret;

    #[test]
    fn the_example_config_parses() {
        toml::from_str::<NbdNetbootSupervisorConfig>(include_str!("../config.example.toml"))
            .unwrap();
    }

    fn digest(n: u8) -> Digest {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
            .parse()
            .unwrap()
    }

    #[derive(Debug)]
    struct StubStore {
        root: PathBuf,
        manifest: Option<ImageManifest>,
        pinned: Mutex<Vec<String>>,
        unpinned: Mutex<Vec<String>>,
    }

    impl StubStore {
        fn new(root: PathBuf, manifest: Option<ImageManifest>) -> Self {
            StubStore {
                root,
                manifest,
                pinned: Mutex::new(Vec::new()),
                unpinned: Mutex::new(Vec::new()),
            }
        }

        fn pinned(&self) -> Vec<String> {
            self.pinned.lock().unwrap().clone()
        }

        fn unpinned(&self) -> Vec<String> {
            self.unpinned.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ImageStore for StubStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<()> {
            Ok(())
        }

        async fn manifest(&self, _: &Digest) -> Result<ImageManifest> {
            self.manifest
                .clone()
                .ok_or_else(|| anyhow!("the stub store serves no manifest"))
        }

        fn blob_path(&self, digest: &Digest) -> PathBuf {
            self.root.join(digest.encoded().replace(':', "-"))
        }

        async fn pin(&self, _: &Digest, job_id: &str) -> Result<()> {
            self.pinned.lock().unwrap().push(job_id.to_string());
            Ok(())
        }

        async fn unpin(&self, job_id: &str) -> Result<()> {
            self.unpinned.lock().unwrap().push(job_id.to_string());
            Ok(())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Overlay(PathBuf, u64),
        Spawn(PathBuf, Vec<String>, StdioMode),
        Interrupt(PathBuf),
        Kill(PathBuf),
    }

    #[derive(Debug, Default, Clone)]
    struct StubLauncher {
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl StubLauncher {
        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }

        fn spawned(&self) -> Vec<(PathBuf, Vec<String>, StdioMode)> {
            self.events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Spawn(program, args, stdio) => Some((program, args, stdio)),
                    _ => None,
                })
                .collect()
        }

        fn record(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    struct StubProcess {
        program: PathBuf,
        launcher: StubLauncher,
        exited: Arc<Notify>,
        done: bool,
        stdout: Option<BoxedAsyncRead>,
        stderr: Option<BoxedAsyncRead>,
    }

    #[async_trait]
    impl WorkloadProcess for StubProcess {
        async fn wait(&mut self) -> std::io::Result<ExitStatus> {
            if !self.done {
                self.exited.notified().await;
                self.done = true;
            }
            Ok(ExitStatus::default())
        }

        fn take_stdout(&mut self) -> Option<BoxedAsyncRead> {
            self.stdout.take()
        }

        fn take_stderr(&mut self) -> Option<BoxedAsyncRead> {
            self.stderr.take()
        }

        async fn kill(&mut self) -> std::io::Result<()> {
            self.launcher.record(Event::Kill(self.program.clone()));
            self.exited.notify_one();
            Ok(())
        }

        async fn interrupt(&mut self) -> std::io::Result<()> {
            self.launcher.record(Event::Interrupt(self.program.clone()));
            self.exited.notify_one();
            Ok(())
        }
    }

    #[async_trait]
    impl ProcessLauncher for StubLauncher {
        async fn create_overlay_no_backing(&self, path: &Path, size: u64) -> Result<()> {
            self.record(Event::Overlay(path.to_path_buf(), size));
            Ok(())
        }

        async fn spawn(
            &self,
            program: &Path,
            args: &[String],
            _cwd: Option<&Path>,
            stdio: StdioMode,
        ) -> Result<Box<dyn WorkloadProcess>> {
            self.record(Event::Spawn(program.to_path_buf(), args.to_vec(), stdio));
            let captured = || -> Option<BoxedAsyncRead> {
                (stdio == StdioMode::Capture)
                    .then(|| Box::new(std::io::Cursor::new(Vec::new())) as _)
            };
            Ok(Box::new(StubProcess {
                program: program.to_path_buf(),
                launcher: self.clone(),
                exited: Arc::new(Notify::new()),
                done: false,
                stdout: captured(),
                stderr: captured(),
            }))
        }
    }

    struct Fixture {
        backend: NbdNetbootBackend,
        store: Arc<StubStore>,
        launcher: StubLauncher,
        _nbd_listener: std::net::TcpListener,
        tmp: TempDir,
    }

    fn config(tmp: &Path, nbd_port: u16) -> NbdNetbootConfig {
        NbdNetbootConfig {
            qemu_storage_daemon_binary: PathBuf::from("/stub/qemu-storage-daemon"),
            qemu_img_binary: PathBuf::from("/stub/qemu-img"),
            nbdfatftpd_binary: PathBuf::from("/stub/nbdfatftpd"),
            state_dir: tmp.join("state"),
            working_disk_max_bytes: 4 * GIB,
            daemon_api_listen_addr: "127.0.0.1:3859".parse().unwrap(),
            nbd_server_listen_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), nbd_port),
            tftp_listen_addr: "127.0.0.1:6969".parse().unwrap(),
            serial_console: None,
            job_retention: RetentionConfig::default(),
            start_script: None,
            stop_script: None,
        }
    }

    fn fixture(manifest: Option<ImageManifest>) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let nbd_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let store = Arc::new(StubStore::new(tmp.path().join("blobs"), manifest));
        let launcher = StubLauncher::default();
        let backend = NbdNetbootBackend::new(
            store.clone(),
            Arc::new(launcher.clone()),
            config(tmp.path(), nbd_listener.local_addr().unwrap().port()),
        );
        Fixture {
            backend,
            store,
            launcher,
            _nbd_listener: nbd_listener,
            tmp,
        }
    }

    fn root_layer(d: Digest, virtual_size: u64, lower: Option<Digest>) -> ImageLayer {
        ImageLayer {
            digest: d,
            size: 10,
            media_type: media_types::DISK_QCOW2.to_string(),
            role: Some(Role::Root),
            virtual_size: Some(virtual_size),
            lower,
        }
    }

    fn boot_layer(d: Digest, virtual_size: u64) -> ImageLayer {
        ImageLayer {
            digest: d,
            size: 10,
            media_type: media_types::DISK_QCOW2.to_string(),
            role: Some(Role::Boot),
            virtual_size: Some(virtual_size),
            lower: None,
        }
    }

    fn image(layers: Vec<ImageLayer>, head: Digest) -> TreadmillImage {
        TreadmillImage {
            layers,
            head,
            title: None,
            version: None,
            description: None,
            base_name: None,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;
    const BOOT_BYTES: u64 = 512 * 1024 * 1024;

    fn netboot_image() -> NetbootImage {
        let (base, head, boot) = (digest(1), digest(2), digest(3));
        NetbootImage {
            image: image(
                vec![
                    root_layer(base, GIB, None),
                    root_layer(head, 2 * GIB, Some(base)),
                    boot_layer(boot, BOOT_BYTES),
                ],
                head,
            ),
            boot: boot_layer(boot, BOOT_BYTES),
        }
    }

    fn start_msg(job_id: Uuid, streaming: bool) -> StartJobMessage {
        StartJobMessage {
            job_id,
            image_spec: ImageSpecification::Image {
                manifest_digest: digest(9),
                locations: vec![ImageLocation {
                    registry: "127.0.0.1:0".to_string(),
                    repository: "treadmill/stub".to_string(),
                }],
            },
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            log_streaming: streaming.then(|| LogStreamingDispatch {
                nats_url: "nats://127.0.0.1:4222".to_string(),
                subject_prefix: format!("logs.{job_id}"),
                write_token: Secret::new("stub".to_string()),
                console_input_subject: None,
                inbox_prefix: None,
            }),
            job_token: None,
        }
    }

    #[test]
    fn an_image_needs_exactly_one_fat_boot_layer() {
        let (base, boot, other) = (digest(1), digest(3), digest(4));

        let none = image(vec![root_layer(base, GIB, None)], base);
        assert!(super::boot_layer(&none).is_err());

        let two = image(
            vec![
                root_layer(base, GIB, None),
                boot_layer(boot, BOOT_BYTES),
                boot_layer(other, BOOT_BYTES),
            ],
            base,
        );
        assert!(super::boot_layer(&two).is_err());

        let mut not_qcow2 = boot_layer(boot, BOOT_BYTES);
        not_qcow2.media_type = "application/octet-stream".to_string();
        let wrong_type = image(vec![root_layer(base, GIB, None), not_qcow2], base);
        assert!(super::boot_layer(&wrong_type).is_err());

        let mut no_virtual_size = boot_layer(boot, BOOT_BYTES);
        no_virtual_size.virtual_size = None;
        let no_size = image(vec![root_layer(base, GIB, None), no_virtual_size], base);
        assert!(super::boot_layer(&no_size).is_err());

        let one = image(
            vec![root_layer(base, GIB, None), boot_layer(boot, BOOT_BYTES)],
            base,
        );
        assert_eq!(super::boot_layer(&one).unwrap().digest, boot);
    }

    #[tokio::test]
    async fn a_manifest_without_a_boot_layer_is_an_invalid_image() {
        let json = r#"{
          "schemaVersion": 2,
          "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "artifactType": "application/vnd.treadmill.image.v1+json",
          "config": { "mediaType": "application/vnd.oci.empty.v1+json",
                      "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                      "size": 2 },
          "layers": [
            { "mediaType": "application/vnd.treadmill.disk.qcow2",
              "digest": "sha256:0101010101010101010101010101010101010101010101010101010101010101",
              "size": 10,
              "annotations": { "dev.treadmill.role": "root", "dev.treadmill.qcow2.virtual-size": "1073741824" } }
          ],
          "annotations": { "dev.treadmill.qcow2.head": "sha256:0101010101010101010101010101010101010101010101010101010101010101" }
        }"#;
        let f = fixture(Some(serde_json::from_str(json).unwrap()));

        let error = f
            .backend
            .fetch(&start_msg(Uuid::new_v4(), false))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::ImageInvalid),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn allocation_serves_both_exports_before_the_host_is_powered_on() {
        let f = fixture(None);
        let job = start_msg(Uuid::new_v4(), true);
        let mut vars = JobVars::new();

        let servers = f
            .backend
            .allocate(&job, f.tmp.path(), netboot_image(), &mut vars)
            .await
            .unwrap();

        let root_overlay = f.tmp.path().join("root.qcow2");
        let boot_overlay = f.tmp.path().join("boot.qcow2");
        let events = f.launcher.events();
        assert_eq!(events[0], Event::Overlay(root_overlay.clone(), 4 * GIB));
        assert_eq!(events[1], Event::Overlay(boot_overlay.clone(), BOOT_BYTES));

        let nbd_port = f.backend.config.nbd_server_listen_addr.port();
        let spawned = f.launcher.spawned();
        let (program, args, stdio) = &spawned[0];
        assert_eq!(program, Path::new("/stub/qemu-storage-daemon"));
        assert_eq!(*stdio, StdioMode::Capture);
        let expected_nodes = BackingChain::new(
            vec![f.store.blob_path(&digest(1)), f.store.blob_path(&digest(2))],
            root_overlay,
        )
        .blockdev_args()
        .into_iter()
        .chain(
            BackingChain::with_prefix(
                BOOT_NODE_PREFIX,
                vec![f.store.blob_path(&digest(3))],
                boot_overlay,
            )
            .blockdev_args(),
        )
        .flat_map(|node| ["--blockdev".to_string(), node])
        .collect::<Vec<_>>();
        let (nodes, server) = args.split_at(expected_nodes.len());
        assert_eq!(nodes, expected_nodes);
        assert_eq!(
            server,
            [
                "--nbd-server".to_string(),
                format!("addr.type=inet,addr.host=0.0.0.0,addr.port={nbd_port},max-connections=0"),
                "--export".to_string(),
                "type=nbd,id=root,node-name=tml-disk,name=root,writable=on".to_string(),
                "--export".to_string(),
                "type=nbd,id=boot,node-name=tml-boot-disk,name=boot,writable=on".to_string(),
            ],
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            while f.launcher.spawned().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (program, args, stdio) = &f.launcher.spawned()[1];
        assert_eq!(program, Path::new("/stub/nbdfatftpd"));
        assert_eq!(*stdio, StdioMode::Capture);
        assert_eq!(
            args,
            &[
                "-l",
                "127.0.0.1:6969",
                "-n",
                &format!("nbd://127.0.0.1:{nbd_port}/boot"),
            ]
        );

        assert_eq!(
            vars.get("nbd_server_listen_addr").map(String::as_str),
            Some(format!("0.0.0.0:{nbd_port}").as_str()),
        );
        assert_eq!(
            vars.get("tftp_listen_addr").map(String::as_str),
            Some("127.0.0.1:6969"),
        );

        drop(servers);
    }

    fn resumable_manifest() -> ImageManifest {
        assemble::build_manifest(
            &[
                assemble::LayerSpec {
                    digest: digest(1),
                    size: 10,
                    role: Role::Root,
                    virtual_size: Some(GIB),
                },
                assemble::LayerSpec {
                    digest: digest(2),
                    size: 10,
                    role: Role::Root,
                    virtual_size: Some(2 * GIB),
                },
                assemble::LayerSpec {
                    digest: digest(3),
                    size: 10,
                    role: Role::Boot,
                    virtual_size: Some(BOOT_BYTES),
                },
            ],
            &assemble::ImageMeta::default(),
        )
        .unwrap()
    }

    async fn retired_workdir(f: &Fixture) -> PathBuf {
        let workdir = f.tmp.path().join("retired");
        std::fs::create_dir(&workdir).unwrap();
        std::fs::write(workdir.join(ROOT_OVERLAY_FILE), b"the root disk").unwrap();
        std::fs::write(workdir.join(BOOT_OVERLAY_FILE), b"the boot disk").unwrap();
        AllocationRecord::new(
            digest(9),
            vec![ImageLocation {
                registry: "127.0.0.1:0".to_string(),
                repository: "treadmill/stub".to_string(),
            }],
            [
                (ROOT_EXPORT.to_string(), ROOT_OVERLAY_FILE.to_string()),
                (BOOT_EXPORT.to_string(), BOOT_OVERLAY_FILE.to_string()),
            ],
        )
        .write(&workdir)
        .await
        .unwrap();
        workdir
    }

    #[tokio::test]
    async fn allocation_records_what_a_resume_needs() {
        let f = fixture(None);
        let job = start_msg(Uuid::new_v4(), true);
        let mut vars = JobVars::new();

        let servers = f
            .backend
            .allocate(&job, f.tmp.path(), netboot_image(), &mut vars)
            .await
            .unwrap();

        let record = AllocationRecord::read(f.tmp.path()).await.unwrap();
        assert_eq!(record.manifest_digest, digest(9));
        assert_eq!(
            record.overlays.get(ROOT_EXPORT).map(String::as_str),
            Some(ROOT_OVERLAY_FILE),
        );
        assert_eq!(
            record.overlays.get(BOOT_EXPORT).map(String::as_str),
            Some(BOOT_OVERLAY_FILE),
        );

        drop(servers);
    }

    /// Adopting a retired job serves its existing disks. Creating the overlays
    /// again is `qemu-img create`, which would discard everything the job
    /// wrote.
    #[tokio::test]
    async fn a_job_takes_an_image_lease_that_outlives_it() {
        let f = fixture(Some(resumable_manifest()));
        let job_id = Uuid::new_v4();

        f.backend.fetch(&start_msg(job_id, false)).await.unwrap();

        assert_eq!(f.store.pinned(), vec![job_id.to_string()]);
        assert!(
            f.store.unpinned().is_empty(),
            "the lease is released when the working directory is collected, not here",
        );
    }

    #[tokio::test]
    async fn adopting_reuses_the_existing_overlays() {
        let f = fixture(Some(resumable_manifest()));
        let workdir = retired_workdir(&f).await;
        let job_id = Uuid::new_v4();
        let job = start_msg(job_id, true);
        let mut vars = JobVars::new();

        let servers = f.backend.adopt(&job, &workdir, &mut vars).await.unwrap();

        assert_eq!(f.store.pinned(), vec![job_id.to_string()]);

        assert!(
            !f.launcher
                .events()
                .iter()
                .any(|event| matches!(event, Event::Overlay(..))),
            "a resume must not re-create the working disks",
        );
        assert_eq!(
            std::fs::read(workdir.join(ROOT_OVERLAY_FILE)).unwrap(),
            b"the root disk",
        );
        assert_eq!(
            std::fs::read(workdir.join(BOOT_OVERLAY_FILE)).unwrap(),
            b"the boot disk",
        );

        let spawned = f.launcher.spawned();
        let (program, args, _) = &spawned[0];
        assert_eq!(program, Path::new("/stub/qemu-storage-daemon"));
        let args = args.join(" ");
        for overlay in [ROOT_OVERLAY_FILE, BOOT_OVERLAY_FILE] {
            assert!(
                args.contains(&workdir.join(overlay).display().to_string()),
                "{args}",
            );
        }
        assert!(args.contains(&f.store.blob_path(&digest(3)).display().to_string()));
        assert!(vars.contains_key("daemon_api_listen_addr"));

        drop(servers);
    }

    #[tokio::test]
    async fn adopting_an_incomplete_retired_directory_cannot_resume() {
        let f = fixture(Some(resumable_manifest()));

        let no_record = f.tmp.path().join("no-record");
        std::fs::create_dir(&no_record).unwrap();

        let no_overlay = retired_workdir(&f).await;
        std::fs::remove_file(no_overlay.join(BOOT_OVERLAY_FILE)).unwrap();

        for workdir in [no_record, no_overlay] {
            let mut vars = JobVars::new();
            let error = f
                .backend
                .adopt(&start_msg(Uuid::new_v4(), false), &workdir, &mut vars)
                .await
                .unwrap_err();
            assert!(
                matches!(error.error_kind, JobErrorKind::CannotResume),
                "{error:?}",
            );
        }
    }

    #[tokio::test]
    async fn an_image_larger_than_the_working_disk_is_refused() {
        let f = fixture(None);
        let mut image = netboot_image();
        image.image.layers[1].virtual_size = Some(8 * GIB);

        let error = f
            .backend
            .allocate(
                &start_msg(Uuid::new_v4(), false),
                f.tmp.path(),
                image,
                &mut JobVars::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::ImageInvalid),
            "{error:?}"
        );
        assert!(f.launcher.events().is_empty());
    }

    #[tokio::test]
    async fn a_storage_daemon_that_never_listens_fails_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(StubStore::new(tmp.path().join("blobs"), None));
        let launcher = StubLauncher::default();
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = unused.local_addr().unwrap().port();
        drop(unused);
        let backend =
            NbdNetbootBackend::new(store, Arc::new(launcher.clone()), config(tmp.path(), port));

        tokio::time::pause();
        let error = backend
            .allocate(
                &start_msg(Uuid::new_v4(), false),
                tmp.path(),
                netboot_image(),
                &mut JobVars::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, JobErrorKind::InternalError),
            "{error:?}"
        );
        assert!(
            launcher.events().contains(&Event::Interrupt(PathBuf::from(
                "/stub/qemu-storage-daemon"
            ))),
            "{:?}",
            launcher.events(),
        );
        assert_eq!(launcher.spawned().len(), 1);
    }

    #[tokio::test]
    async fn teardown_stops_the_tftp_server_before_the_storage_daemon() {
        let f = fixture(None);
        let job = start_msg(Uuid::new_v4(), true);
        let mut vars = JobVars::new();

        let servers = f
            .backend
            .allocate(&job, f.tmp.path(), netboot_image(), &mut vars)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while f.launcher.spawned().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let Workload {
            mut process,
            serial,
            channels,
        } = f
            .backend
            .launch(&job, f.tmp.path(), servers, &vars)
            .await
            .unwrap();
        assert!(serial.is_none());
        assert_eq!(
            channels
                .iter()
                .map(|(channel, _)| channel.to_string())
                .collect::<Vec<_>>(),
            [
                "storage-daemon-stdout",
                "storage-daemon-stderr",
                "nbdfatftpd"
            ],
        );

        process.kill().await.unwrap();
        process.wait().await.unwrap();

        let interrupts = f
            .launcher
            .events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Interrupt(program) => Some(program),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            interrupts,
            [
                PathBuf::from("/stub/nbdfatftpd"),
                PathBuf::from("/stub/qemu-storage-daemon"),
            ],
        );
        assert_eq!(f.launcher.spawned().len(), 2);
    }

    #[tokio::test]
    async fn without_log_streaming_nothing_is_captured() {
        let f = fixture(None);
        let job = start_msg(Uuid::new_v4(), false);
        let mut vars = JobVars::new();

        let servers = f
            .backend
            .allocate(&job, f.tmp.path(), netboot_image(), &mut vars)
            .await
            .unwrap();
        let workload = f
            .backend
            .launch(&job, f.tmp.path(), servers, &vars)
            .await
            .unwrap();
        assert!(workload.channels.is_empty());
        assert!(
            f.launcher
                .spawned()
                .iter()
                .all(|(_, _, stdio)| *stdio == StdioMode::Inherit)
        );
    }

    mod real_daemons {
        use super::*;

        use std::net::UdpSocket;

        use treadmill_supervisor_lib::launcher::CliLauncher;

        fn which(bin: &str) -> Option<PathBuf> {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .map(|dir| dir.join(bin))
                .find(|c| c.is_file())
        }

        struct Tools {
            qemu_img: PathBuf,
            mkfs_vfat: PathBuf,
            mcopy: PathBuf,
        }

        fn tools() -> Option<Tools> {
            which("qemu-storage-daemon")?;
            which("nbdfatftpd")?;
            Some(Tools {
                qemu_img: which("qemu-img")?,
                mkfs_vfat: which("mkfs.vfat")?,
                mcopy: which("mcopy")?,
            })
        }

        fn run(cmd: &mut std::process::Command) {
            let out = cmd.output().expect("spawn tool");
            assert!(
                out.status.success(),
                "{cmd:?} failed: {}",
                String::from_utf8_lossy(&out.stderr),
            );
        }

        fn free_port(udp: bool) -> u16 {
            if udp {
                UdpSocket::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
                    .port()
            } else {
                std::net::TcpListener::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
                    .port()
            }
        }

        fn tftp_get(server: SocketAddr, file: &str) -> Result<Vec<u8>, String> {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut rrq = vec![0, 1];
            rrq.extend_from_slice(file.as_bytes());
            rrq.push(0);
            rrq.extend_from_slice(b"octet\0");
            socket.send_to(&rrq, server).unwrap();

            let mut data = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let (n, peer) = socket.recv_from(&mut buf).map_err(|e| e.to_string())?;
                let packet = &buf[..n];
                if packet[1] != 3 {
                    return Err(format!("unexpected TFTP packet: {packet:?}"));
                }
                data.extend_from_slice(&packet[4..]);
                socket.send_to(&[0, 4, packet[2], packet[3]], peer).unwrap();
                if n < 4 + 512 {
                    return Ok(data);
                }
            }
        }

        #[tokio::test]
        async fn the_boot_layer_is_served_over_tftp() {
            let Some(t) = tools() else {
                eprintln!(
                    "qemu-storage-daemon, nbdfatftpd, qemu-img, mkfs.vfat or mcopy not on PATH; skipping"
                );
                return;
            };

            let tmp = tempfile::tempdir().unwrap();
            let blobs = tmp.path().join("blobs");
            std::fs::create_dir(&blobs).unwrap();
            let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
            let payload_file = tmp.path().join("kernel.img");
            std::fs::write(&payload_file, &payload).unwrap();

            let boot_fat = tmp.path().join("boot.fat");
            run(std::process::Command::new(&t.mkfs_vfat)
                .args(["-C", "-F", "32"])
                .arg(&boot_fat)
                .arg("65536"));
            run(std::process::Command::new(&t.mcopy)
                .arg("-i")
                .arg(&boot_fat)
                .arg(&payload_file)
                .arg("::kernel.img"));
            let boot_size = std::fs::metadata(&boot_fat).unwrap().len();
            let boot_blob = blobs.join(digest(3).encoded().replace(':', "-"));
            run(std::process::Command::new(&t.qemu_img)
                .args(["convert", "-f", "raw", "-O", "qcow2"])
                .arg(&boot_fat)
                .arg(&boot_blob));

            let base_blob = blobs.join(digest(1).encoded().replace(':', "-"));
            run(std::process::Command::new(&t.qemu_img)
                .args(["create", "-f", "qcow2"])
                .arg(&base_blob)
                .arg(GIB.to_string()));

            let nbd_port = free_port(false);
            let tftp_port = free_port(true);
            let mut config = config(tmp.path(), nbd_port);
            config.qemu_storage_daemon_binary = PathBuf::from("qemu-storage-daemon");
            config.qemu_img_binary = t.qemu_img.clone();
            config.nbdfatftpd_binary = PathBuf::from("nbdfatftpd");
            config.nbd_server_listen_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), nbd_port);
            config.tftp_listen_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), tftp_port);

            let store = Arc::new(StubStore::new(blobs, None));
            let backend =
                NbdNetbootBackend::new(store, Arc::new(CliLauncher::new(&t.qemu_img)), config);

            let image = NetbootImage {
                image: image(
                    vec![
                        root_layer(digest(1), GIB, None),
                        boot_layer(digest(3), boot_size),
                    ],
                    digest(1),
                ),
                boot: boot_layer(digest(3), boot_size),
            };
            let job = start_msg(Uuid::new_v4(), true);
            let workdir = tmp.path().join("job");
            std::fs::create_dir(&workdir).unwrap();
            let mut vars = JobVars::new();
            let servers = backend
                .allocate(&job, &workdir, image, &mut vars)
                .await
                .unwrap();
            let mut workload = backend
                .launch(&job, &workdir, servers, &vars)
                .await
                .unwrap();
            for (_, reader) in workload.channels {
                tokio::spawn(async move {
                    let mut reader = reader;
                    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
                });
            }

            let tftp_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), tftp_port);
            let fetched = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                loop {
                    match tftp_get(tftp_addr, "kernel.img") {
                        Ok(data) => return data,
                        Err(e) if std::time::Instant::now() < deadline => {
                            eprintln!("TFTP fetch not yet served: {e}");
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        Err(e) => panic!("TFTP fetch never served: {e}"),
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(fetched, payload);

            workload.process.kill().await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), workload.process.wait())
                .await
                .expect("the storage daemon exits after teardown")
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while tokio::net::TcpStream::connect(("127.0.0.1", nbd_port))
                    .await
                    .is_ok()
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("the NBD port is released after teardown");
        }
    }
}
