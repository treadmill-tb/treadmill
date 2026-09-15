//! The QEMU supervisor runs each job in a virtual machine.
//!
//! # Image contract
//!
//! An image for this supervisor provides exactly one role, `disk`: a
//! whole, partitioned disk (qcow2 layers, over a qcow2 or raw base). The
//! supervisor layers a per-job writable overlay of `working_disk_max_bytes` on
//! top, and the configured QEMU invocation attaches it via `{disk_node}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::{
    ImageLocation, ImageSpecification, LogChannel, LogFormat, LogRender, LogView,
};
use treadmill_rs::connector::{self, StartJobMessage, SupervisorConnector};
use treadmill_rs::image::Digest;
use treadmill_rs::image::blockdev::BackingChain;
use treadmill_rs::image::parse::{self, TreadmillImage};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_supervisor_lib::bootstrap::{self, COORD_MAILBOX_CAPACITY, OnDisconnect, StopSignal};
use treadmill_supervisor_lib::capture::{SerialConsole, SerialSocket};
use treadmill_supervisor_lib::job::{JobBackend, JobRunner, JobRunnerConfig, JobVars, Workload};
use treadmill_supervisor_lib::job_log::{self, JobLogRegistry};
use treadmill_supervisor_lib::launcher::{self, ProcessLauncher, StdioMode, WorkloadProcess};
use treadmill_supervisor_lib::leases;
use treadmill_supervisor_lib::oci_store::{ImageStore, Location, OciStore, OciStoreConfig};
use treadmill_supervisor_lib::publisher::LogPublisherConfig;
use treadmill_supervisor_lib::workdirs::{AllocationRecord, JobWorkdirs, RetentionConfig};

const QEMU_STDOUT: LogChannel = LogChannel::from_static("qemu-stdout");
const QEMU_STDERR: LogChannel = LogChannel::from_static("qemu-stderr");

/// The role of the disk the VM boots from.
const DISK_ROLE: &str = "disk";

/// Prefix of the `-blockdev` node names of the disk's chain.
const DISK_NODE_PREFIX: &str = "tml";

const DISK_OVERLAY_FILE: &str = "overlay.qcow2";

#[derive(Parser, Debug, Clone)]
pub struct QemuSupervisorArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,

    /// Per-job inputs for the switchboard-less `local` connector
    /// (`coord_connector = "local"`). Ignored by the other connectors.
    #[command(flatten)]
    local_job: Option<treadmill_local_connector::LocalJobArgs>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuConfig {
    /// Main QEMU binary to execute for a job.
    qemu_binary: PathBuf,

    /// `qemu-img` binary, to work with qcow2 files.
    qemu_img_binary: PathBuf,

    /// Directory this supervisor keeps its per-job working directories in.
    state_dir: PathBuf,

    /// List of arguments to pass to the QEMU binary.
    ///
    /// These arguments support template strings using the
    /// [`strfmt`](https://docs.rs/strfmt/latest/strfmt/) crate.
    ///
    /// The available template strings are:
    ///
    /// - `job_id`: UUID as a hyphenated string
    ///
    /// - `job_workdir`: per-job state directory
    ///
    /// - `disk_node`: `node-name` of the writable top of the runtime backing
    ///   chain of the image's `disk` ([`BackingChain::top_node`]).
    ///
    ///   The supervisor internally prepends the `-blockdev` nodes assembling
    ///   the chain to the invocation, so the configured args should attach the
    ///   disk device by referencing this node, e.g. `-device
    ///   virtio-blk-device,drive={disk_node}`.
    ///
    /// - `daemon_api_listen_addr`: the address the per-job daemon API is bound
    ///   to, with an IPv6 address enclosed in square brackets, e.g.
    ///   `[::1]:8080`.
    ///
    ///   This is the supervisor's listen address, not necessarily one the guest
    ///   can reach (e.g., it might be bound to the "any interface IP" `0.0.0.0`).
    ///
    /// Any variable the start script emits (`tml-set-variable:<key>=<value>`)
    /// can be substituted too; the hook runs before the arguments are
    /// templated.
    ///
    /// A literal brace in an argument must be doubled (`{{`/`}}`); a `{name}`
    /// referencing a variable that is not set causes a job launch error.
    qemu_args: Vec<String>,

    /// Maximum "working" disk image to be allocated for a job, in bytes.
    ///
    /// The image top layers are thinly provisioned qcow2 CoW images. This sets
    /// their top-level size, which has to be at least as large as the next
    /// lower layer. This space will not be directly allocated, but is usable by
    /// the VMs.
    ///
    /// Launching jobs with images that have a top-most layer larger than this
    /// value will fail.
    working_disk_max_bytes: u64,

    daemon_api_listen_addr: std::net::SocketAddr,

    /// Retention of the working directories of removed jobs.
    #[serde(default)]
    job_retention: RetentionConfig,

    start_script: Option<PathBuf>,

    // TODO: add tests exercising the stop script, with failures at various
    // parts throughout the job lifecycle
    stop_script: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configuration of the web-socket connector. Required only if used.
    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    /// Local OCI store (per-server Zot daemon) the supervisor pulls images from
    /// and reads blob files out of directly.
    oci_store: OciStoreConfig,

    /// Configuration of the log-streaming subsystem.
    #[serde(default)]
    log_streaming: LogPublisherConfig,

    qemu: QemuConfig,
}

#[derive(Debug)]
pub struct QemuBackend {
    /// Read-only client of the local OCI store daemon (per-server Zot).
    image_store: Arc<dyn ImageStore>,

    /// Swappable process launcher, such that the supervisor can be unit-tested
    /// without starting real QEMU binaries:
    launcher: Arc<dyn ProcessLauncher>,

    config: QemuConfig,
}

impl QemuBackend {
    pub fn new(
        image_store: Arc<dyn ImageStore>,
        launcher: Arc<dyn ProcessLauncher>,
        config: QemuConfig,
    ) -> Self {
        QemuBackend {
            image_store,
            launcher,
            config,
        }
    }

    async fn resolve_image(
        &self,
        job_id: Uuid,
        manifest_digest: &Digest,
        locations: &[ImageLocation],
    ) -> Result<TreadmillImage, connector::JobError> {
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
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
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
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Cannot retrieve image manifest of {manifest_digest}: {e:#}",),
            })?;

        let image = parse::parse_image(&manifest).map_err(|e| connector::JobError {
            error_kind: connector::JobErrorKind::ImageInvalid,
            description: format!("Image {manifest_digest} is not a valid Treadmill image: {e}"),
        })?;

        image
            .check_roles(&[DISK_ROLE])
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::ImageNotCompatible,
                description: format!("Image {manifest_digest} cannot boot in a VM: {e}"),
            })?;

        Ok(image)
    }

    /// Resolve the image's `disk` chain onto `overlay_file`.
    fn disk_chain(
        &self,
        image: &TreadmillImage,
        overlay_file: &Path,
    ) -> Result<BackingChain, connector::JobError> {
        let disk = image.chain(DISK_ROLE).ok_or_else(|| connector::JobError {
            error_kind: connector::JobErrorKind::ImageNotCompatible,
            description: format!("Image provides no {DISK_ROLE} role"),
        })?;

        let chain = BackingChain::from_chain(
            DISK_NODE_PREFIX,
            &disk,
            |digest| self.image_store.blob_path(digest),
            overlay_file,
        )
        .map_err(|e| connector::JobError {
            error_kind: connector::JobErrorKind::ImageNotCompatible,
            description: format!("Cannot attach the image's {DISK_ROLE}: {e}"),
        })?;
        let head_virtual_size = disk
            .virtual_size()
            .expect("a chain of a known format has a virtual size");

        // The overlay is always created with exactly `working_disk_max_bytes`.
        // Fail if the backing image's head layer is smaller than this. If we'd
        // clamp it, we'd risk silently cutting off referenced data.
        if head_virtual_size > self.config.working_disk_max_bytes {
            return Err(connector::JobError {
                error_kind: connector::JobErrorKind::ImageInvalid,
                description: format!(
                    "Image head virtual size ({} byte) exceeds the working-disk \
                     maximum ({} byte)",
                    head_virtual_size, self.config.working_disk_max_bytes,
                ),
            });
        }

        Ok(chain)
    }

    fn seed_vars(&self, vars: &mut JobVars, chain: &BackingChain) {
        // The disk is attached by referencing the writable top node of the
        // backing chain the supervisor prepends as `-blockdev` args at launch.
        vars.insert("disk_node".to_string(), chain.top_node());
    }
}

fn image_reference(
    job: &StartJobMessage,
) -> Result<(Digest, Vec<ImageLocation>), connector::JobError> {
    match &job.image_spec {
        ImageSpecification::Image {
            manifest_digest,
            locations,
        } => Ok((*manifest_digest, locations.clone())),

        unsupported_image_spec => Err(connector::JobError {
            error_kind: connector::JobErrorKind::ImageNotCompatible,
            description: format!("Unsupported image specification: {unsupported_image_spec:?}",),
        }),
    }
}

fn cannot_resume(e: anyhow::Error) -> connector::JobError {
    connector::JobError {
        error_kind: connector::JobErrorKind::CannotResume,
        description: format!("{e:#}"),
    }
}

#[async_trait]
impl JobBackend for QemuBackend {
    type Image = TreadmillImage;
    type Allocation = BackingChain;

    async fn fetch(&self, job: &StartJobMessage) -> Result<TreadmillImage, connector::JobError> {
        let (manifest_digest, locations) = image_reference(job)?;
        self.resolve_image(job.job_id, &manifest_digest, &locations)
            .await
    }

    #[instrument(skip(self, job, image, vars), err(Debug, level = Level::WARN))]
    async fn allocate(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        image: TreadmillImage,
        vars: &mut JobVars,
    ) -> Result<BackingChain, connector::JobError> {
        let overlay_file = workdir.join(DISK_OVERLAY_FILE);
        let chain = self.disk_chain(&image, &overlay_file)?;

        event!(
            Level::DEBUG,
            ?overlay_file,
            virtual_size_bytes = self.config.working_disk_max_bytes,
            "Creating per-job overlay disk"
        );
        self.launcher
            .create_overlay_no_backing(&overlay_file, self.config.working_disk_max_bytes)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Failed to allocate disk image: {e:#}"),
            })?;

        let (manifest_digest, locations) = image_reference(job)?;
        AllocationRecord::new(
            manifest_digest,
            locations,
            [(DISK_ROLE.to_string(), DISK_OVERLAY_FILE.to_string())],
        )
        .write(workdir)
        .await
        .map_err(|e| connector::JobError {
            error_kind: connector::JobErrorKind::InternalError,
            description: format!("Failed to record the job's allocation: {e:#}"),
        })?;

        self.seed_vars(vars, &chain);

        Ok(chain)
    }

    #[instrument(skip(self, job, vars), err(Debug, level = Level::WARN))]
    async fn adopt(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        vars: &mut JobVars,
    ) -> Result<BackingChain, connector::JobError> {
        let record = AllocationRecord::read(workdir)
            .await
            .map_err(cannot_resume)?;
        let overlay_file = record
            .overlay(workdir, DISK_ROLE)
            .await
            .map_err(cannot_resume)?;

        event!(
            Level::INFO,
            ?overlay_file,
            manifest_digest = %record.manifest_digest,
            "Adopting the working disk of a retired job"
        );

        let image = self
            .resolve_image(job.job_id, &record.manifest_digest, &record.locations)
            .await?;
        let chain = self.disk_chain(&image, &overlay_file)?;

        self.seed_vars(vars, &chain);

        Ok(chain)
    }

    async fn launch(
        &self,
        job: &StartJobMessage,
        workdir: &Path,
        chain: BackingChain,
        vars: &JobVars,
    ) -> Result<Workload, connector::JobError> {
        let templated_args = self
            .config
            .qemu_args
            .iter()
            .map(|argstr| strfmt::strfmt(argstr, vars))
            .collect::<Result<Vec<String>, strfmt::FmtError>>()
            .map_err(|format_error| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!(
                    "Failed to generate QEMU command line arguments: {format_error:?}",
                ),
            })?;

        // When the dispatch enables log streaming, capture qemu's console
        // output: pipe stdout/stderr (read back by the runner) and route the
        // guest serial console to a unix socket.
        //
        // TODO: currently, when log streaming is disabled, this attaches this
        // process' stdout + stderr to QEMU. Presumably this is not what we
        // want, but we want to keep the logs somewhere. File in the job state
        // dir, maybe?
        if job.log_streaming.is_none() {
            return self
                .spawn_qemu(chain, Vec::new(), templated_args, StdioMode::Inherit)
                .await
                .map(|process| Workload {
                    process,
                    serial: None,
                    channels: Vec::new(),
                });
        }

        let serial_sock_path = workdir.join("serial.sock");
        let (serial, capture_args) = match SerialSocket::bind(&serial_sock_path).await {
            Ok(socket) => {
                // qemu connects to our already-bound listener as the client
                // (`server=off`), so there is no connect race.
                let args = vec![
                    "-chardev".to_string(),
                    format!(
                        "socket,id=tml-serial,path={},server=off",
                        socket.path().display(),
                    ),
                    "-serial".to_string(),
                    "chardev:tml-serial".to_string(),
                ];
                (Some(socket), args)
            }
            Err(e) => {
                event!(
                    Level::WARN,
                    ?serial_sock_path,
                    error = ?e,
                    "Failed to bind the serial capture socket; this job ships no serial channel",
                );
                (None, Vec::new())
            }
        };

        let mut process = self
            .spawn_qemu(chain, capture_args, templated_args, StdioMode::Capture)
            .await?;

        let channels = [
            (QEMU_STDOUT, process.take_stdout()),
            (QEMU_STDERR, process.take_stderr()),
        ]
        .into_iter()
        .filter_map(|(channel, reader)| reader.map(|reader| (channel, reader)))
        .collect();

        Ok(Workload {
            process,
            serial: serial.map(SerialConsole::Listener),
            channels,
        })
    }

    fn log_views(&self) -> Vec<LogView> {
        qemu_log_views()
    }
}

impl QemuBackend {
    async fn spawn_qemu(
        &self,
        chain: BackingChain,
        capture_args: Vec<String>,
        templated_args: Vec<String>,
        stdio_mode: StdioMode,
    ) -> Result<Box<dyn WorkloadProcess>, connector::JobError> {
        let mut qemu_args: Vec<String> = Vec::new();
        for node in chain.blockdev_args() {
            qemu_args.push("-blockdev".to_string());
            qemu_args.push(node);
        }
        qemu_args.extend(capture_args);
        qemu_args.extend(templated_args);

        event!(
            Level::INFO,
            qemu_binary = ?self.config.qemu_binary,
            ?qemu_args,
            "Launching QEMU process",
        );
        self.launcher
            .spawn(&self.config.qemu_binary, &qemu_args, None, stdio_mode)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Failed to launch the QEMU process: {e:#}"),
            })
    }
}

/// The possible log streaming views that the QEMU runner can produce.
///
/// It's OK if this ends up referencing streams that are never allocated /
/// produced (like the serial console, when we fail to bind to the socket). The
/// runner filters these by the channels actually produced.
fn qemu_log_views() -> Vec<LogView> {
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
            id: "qemu".to_string(),
            label: "QEMU process".to_string(),
            render: LogRender::Text,
            format: LogFormat::Raw,
            channels: vec![QEMU_STDOUT, QEMU_STDERR],
            order: 20,
            default: false,
            input: false,
        },
    ]
}

impl QemuSupervisorConfig {
    fn job_runner(&self, workdirs: Arc<JobWorkdirs>, job_log: JobLogRegistry) -> JobRunnerConfig {
        JobRunnerConfig {
            job_address: self.base.job_address,
            workdirs,
            daemon_api_listen_addr: self.qemu.daemon_api_listen_addr,
            job_switchboard_api_url: self.base.job_switchboard_api_url.clone(),
            start_script: self.qemu.start_script.clone(),
            stop_script: self.qemu.stop_script.clone(),
            log_streaming: self.log_streaming.clone(),
            job_log,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = QemuSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file)
        .with_context(|| format!("Reading config file {:?}", args.config_file))?;
    let config: QemuSupervisorConfig = toml::from_str(&config_str)
        .with_context(|| format!("Parsing config file {:?}", args.config_file))?;

    // The subscriber needs the configured job-log threshold, so it goes up
    // after the config is read; anything failing before this is reported by
    // `main` returning it.
    let job_log = job_log::init_tracing(&config.log_streaming.job_log_level)?;
    event!(Level::INFO, "Treadmill Qemu Supervisor, Hello World!");

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.qemu.qemu_img_binary.clone(),
    ));

    let workdirs =
        JobWorkdirs::start(&config.qemu.state_dir, config.qemu.job_retention.clone()).await?;

    leases::spawn_reaper(
        workdirs.clone(),
        image_store.clone(),
        config.qemu.job_retention.sweep_interval,
    );

    let backend = Arc::new(QemuBackend::new(image_store, launcher, config.qemu.clone()));
    let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

    // A one-shot local job has nobody to wait for, so Ctrl-C terminates and
    // removes it.
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
                // One-shot, switchboard-less run: drive a single job from the
                // command-line `LocalJobArgs` against the local OCI store.
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
    //! Direct drive of the QEMU backend: the backing chain it reads off an
    //! image, the overlay it sizes against the working-disk ceiling, and the
    //! invocation it hands to QEMU. The lifecycle these steps hang off is the
    //! runner's, and is tested in `treadmill_supervisor_lib::job`.

    use super::*;

    use std::process::ExitStatus;

    use oci_spec::image::ImageManifest;
    use tempfile::TempDir;
    use uuid::Uuid;

    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, LogStreamingDispatch, RestartPolicy,
    };
    use treadmill_rs::image::Digest;
    use treadmill_rs::image::assemble::{Blob, BlobFormat, ImageBuilder, manifest_bytes};
    use treadmill_rs::util::Secret;
    use treadmill_supervisor_lib::launcher::WorkloadProcess;

    #[test]
    fn the_example_config_parses() {
        toml::from_str::<QemuSupervisorConfig>(include_str!("../config.example.toml")).unwrap();
    }

    /// A distinct, well-formed digest per small integer.
    fn digest(n: u8) -> Digest {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
            .parse()
            .unwrap()
    }

    /// OCI store stub: serves a canned manifest and maps each digest to its own
    /// blob path, so the assembled chain can be read back by layer.
    #[derive(Debug)]
    struct StubStore {
        root: PathBuf,
        manifest: Option<ImageManifest>,
        pinned: std::sync::Mutex<Vec<String>>,
        unpinned: std::sync::Mutex<Vec<String>>,
    }

    impl StubStore {
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
            self.root.join(format!("{digest}.qcow2"))
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

    /// Launcher that records what it was asked to do instead of doing it.
    #[derive(Debug, Default)]
    struct StubLauncher {
        overlays: std::sync::Mutex<Vec<(PathBuf, u64)>>,
        spawned: std::sync::Mutex<Vec<(PathBuf, Vec<String>, StdioMode)>>,
    }

    impl StubLauncher {
        fn overlays(&self) -> Vec<(PathBuf, u64)> {
            self.overlays.lock().unwrap().clone()
        }

        fn spawned_args(&self) -> Vec<String> {
            let spawned = self.spawned.lock().unwrap();
            assert_eq!(spawned.len(), 1, "exactly one process was spawned");
            spawned[0].1.clone()
        }

        fn spawned_stdio(&self) -> StdioMode {
            let spawned = self.spawned.lock().unwrap();
            assert_eq!(spawned.len(), 1, "exactly one process was spawned");
            spawned[0].2
        }
    }

    struct StubProcess;

    #[async_trait]
    impl WorkloadProcess for StubProcess {
        async fn wait(&mut self) -> std::io::Result<ExitStatus> {
            std::future::pending::<std::io::Result<ExitStatus>>().await
        }
        async fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl ProcessLauncher for StubLauncher {
        async fn create_overlay_no_backing(&self, path: &Path, size: u64) -> Result<()> {
            self.overlays
                .lock()
                .unwrap()
                .push((path.to_path_buf(), size));
            Ok(())
        }

        async fn spawn(
            &self,
            program: &Path,
            args: &[String],
            _cwd: Option<&Path>,
            stdio: StdioMode,
        ) -> Result<Box<dyn WorkloadProcess>> {
            self.spawned
                .lock()
                .unwrap()
                .push((program.to_path_buf(), args.to_vec(), stdio));
            Ok(Box::new(StubProcess))
        }
    }

    /// A backend over a temp dir, with the stubs it was built from.
    struct Fixture {
        backend: QemuBackend,
        store: Arc<StubStore>,
        launcher: Arc<StubLauncher>,
        tmp: TempDir,
    }

    fn fixture(working_disk_max_bytes: u64, qemu_args: Vec<&str>) -> Fixture {
        fixture_serving(working_disk_max_bytes, qemu_args, None)
    }

    /// Like [`fixture`], for a backend whose store serves `manifest`.
    fn fixture_serving(
        working_disk_max_bytes: u64,
        qemu_args: Vec<&str>,
        manifest: Option<ImageManifest>,
    ) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(StubStore {
            root: tmp.path().join("blobs"),
            manifest,
            pinned: std::sync::Mutex::new(Vec::new()),
            unpinned: std::sync::Mutex::new(Vec::new()),
        });
        let launcher = Arc::new(StubLauncher::default());

        let config = QemuConfig {
            qemu_binary: PathBuf::from("/nonexistent/qemu"),
            qemu_img_binary: PathBuf::from("/nonexistent/qemu-img"),
            state_dir: tmp.path().join("state"),
            qemu_args: qemu_args.into_iter().map(str::to_string).collect(),
            working_disk_max_bytes,
            daemon_api_listen_addr: "127.0.0.1:3859".parse().unwrap(),
            job_retention: RetentionConfig::default(),
            start_script: None,
            stop_script: None,
        };

        Fixture {
            backend: QemuBackend::new(store.clone(), launcher.clone(), config),
            store,
            launcher,
            tmp,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// An image whose `disk` chain has a qcow2 layer per virtual size, base
    /// first, with the layer digests `digest(1)`, `digest(2)`, ….
    fn disk_image(virtual_sizes: &[u64]) -> TreadmillImage {
        let mut builder = ImageBuilder::default();
        for (n, virtual_size) in (1..).zip(virtual_sizes) {
            builder
                .push(
                    "disk".parse().unwrap(),
                    Blob {
                        digest: digest(n),
                        size: 10,
                        format: BlobFormat::Qcow2 {
                            virtual_size: *virtual_size,
                        },
                    },
                )
                .unwrap();
        }
        builder.build().unwrap()
    }

    /// The disk's chain is mapped to the blob paths the local store holds the
    /// layers at, base first, in the order the `-blockdev` nodes have to be
    /// emitted in. (Resolving the chain is the image's business, and is tested
    /// in `treadmill_rs::image::parse`.)
    #[tokio::test]
    async fn the_disk_chain_is_mapped_to_store_blob_paths() {
        let f = fixture(4 * GIB, vec![]);
        let image = disk_image(&[GIB, 2 * GIB, 4 * GIB]);

        let chain = f
            .backend
            .allocate(
                &start_msg(Uuid::new_v4()),
                f.tmp.path(),
                image,
                &mut JobVars::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            chain.lower_paths().collect::<Vec<_>>(),
            [digest(1), digest(2), digest(3)]
                .map(|d| f.store.blob_path(&d))
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
        );
        assert_eq!(chain.overlay_path(), f.tmp.path().join("overlay.qcow2"));
    }

    async fn allocate(
        f: &Fixture,
        head_virtual_size: u64,
    ) -> Result<BackingChain, connector::JobError> {
        let image = disk_image(&[head_virtual_size]);
        let mut vars = JobVars::new();
        f.backend
            .allocate(&start_msg(Uuid::new_v4()), f.tmp.path(), image, &mut vars)
            .await
    }

    /// The VM is exposed exactly the configured working-disk size, so an image
    /// whose head is larger would have its tail cut off.
    #[tokio::test]
    async fn an_image_larger_than_the_working_disk_is_refused() {
        let f = fixture(4 * GIB, vec![]);

        let error = allocate(&f, 8 * GIB).await.unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::ImageInvalid),
            "{error:?}",
        );
        assert!(f.launcher.overlays().is_empty(), "nothing was allocated");
    }

    /// The overlay is created at the ceiling rather than at the head's size, so
    /// the guest can grow into the whole working disk.
    #[tokio::test]
    async fn the_overlay_is_sized_to_the_working_disk_maximum() {
        let f = fixture(4 * GIB, vec![]);

        allocate(&f, GIB).await.unwrap();

        assert_eq!(
            f.launcher.overlays(),
            vec![(f.tmp.path().join("overlay.qcow2"), 4 * GIB)],
        );
    }

    /// The variables the runner seeds before it calls the backend.
    fn runner_vars(job_id: Uuid, workdir: &Path) -> JobVars {
        JobVars::from([
            ("job_id".to_string(), job_id.to_string()),
            ("job_workdir".to_string(), workdir.display().to_string()),
        ])
    }

    fn start_msg(job_id: Uuid) -> StartJobMessage {
        StartJobMessage {
            job_id,
            image_spec: ImageSpecification::Image {
                manifest_digest: digest(3),
                locations: vec![ImageLocation {
                    registry: "127.0.0.1:0".to_string(),
                    repository: "treadmill/stub".to_string(),
                }],
            },
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            log_streaming: None,
            job_token: None,
        }
    }

    /// The configured invocation attaches the disk by node name; the nodes
    /// assembling the chain have to be on the command line before it, and the
    /// configured args follow verbatim once templated.
    #[tokio::test]
    async fn the_invocation_prepends_the_backing_chain() {
        let f = fixture(
            4 * GIB,
            vec![
                "-name",
                "tml-{job_id}",
                "-device",
                "virtio-blk-pci,drive={disk_node}",
            ],
        );

        let job_id = Uuid::new_v4();
        let mut vars = runner_vars(job_id, f.tmp.path());
        let image = disk_image(&[GIB]);
        let chain = f
            .backend
            .allocate(&start_msg(job_id), f.tmp.path(), image, &mut vars)
            .await
            .unwrap();
        let expected_nodes = chain.blockdev_args();

        f.backend
            .launch(&start_msg(job_id), f.tmp.path(), chain, &vars)
            .await
            .unwrap();

        let args = f.launcher.spawned_args();
        let (nodes, configured) = args.split_at(expected_nodes.len() * 2);

        assert_eq!(
            nodes,
            expected_nodes
                .into_iter()
                .flat_map(|node| ["-blockdev".to_string(), node])
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            configured,
            [
                "-name",
                &format!("tml-{job_id}"),
                "-device",
                "virtio-blk-pci,drive=tml-disk",
            ],
        );
    }

    /// The captured serial console has to be QEMU's first `-serial`, or a
    /// configuration that points one of its own somewhere else takes the
    /// guest's console with it and the capture ships nothing.
    #[tokio::test]
    async fn the_captured_console_precedes_a_configured_serial() {
        let f = fixture(4 * GIB, vec!["-serial", "mon:stdio"]);

        let job_id = Uuid::new_v4();
        let mut vars = runner_vars(job_id, f.tmp.path());
        let image = disk_image(&[GIB]);

        let mut msg = start_msg(job_id);
        msg.log_streaming = Some(LogStreamingDispatch {
            nats_url: "nats://127.0.0.1:4222".to_string(),
            subject_prefix: format!("logs.{job_id}"),
            write_token: Secret::new("stub".to_string()),
            console_input_subject: None,
            inbox_prefix: None,
        });

        let chain = f
            .backend
            .allocate(&msg, f.tmp.path(), image, &mut vars)
            .await
            .unwrap();
        f.backend
            .launch(&msg, f.tmp.path(), chain, &vars)
            .await
            .unwrap();

        let args = f.launcher.spawned_args();
        let serials: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-serial")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(serials, vec!["chardev:tml-serial", "mon:stdio"], "{args:?}");
    }

    /// A serial socket that cannot be bound costs the job its `serial` channel
    /// and nothing else: dropping to inherited stdio would leave the publisher
    /// with no channel at all, so the job would stream nothing.
    #[tokio::test]
    async fn a_serial_bind_failure_still_captures_stdout_and_stderr() {
        let f = fixture(4 * GIB, vec![]);

        let job_id = Uuid::new_v4();
        let mut vars = runner_vars(job_id, f.tmp.path());
        let image = disk_image(&[GIB]);

        let mut msg = start_msg(job_id);
        msg.log_streaming = Some(LogStreamingDispatch {
            nats_url: "nats://127.0.0.1:4222".to_string(),
            subject_prefix: format!("logs.{job_id}"),
            write_token: Secret::new("stub".to_string()),
            console_input_subject: None,
            inbox_prefix: None,
        });

        let chain = f
            .backend
            .allocate(&msg, f.tmp.path(), image, &mut vars)
            .await
            .unwrap();

        // A workdir that does not exist has nowhere to put the socket file.
        let missing_workdir = f.tmp.path().join("gone");
        f.backend
            .launch(&msg, &missing_workdir, chain, &vars)
            .await
            .unwrap();

        assert_eq!(f.launcher.spawned_stdio(), StdioMode::Capture);
        let args = f.launcher.spawned_args();
        assert!(
            !args.iter().any(|a| a == "-chardev" || a == "-serial"),
            "no serial channel is wired up: {args:?}",
        );
    }

    fn resumable_manifest(head_virtual_size: u64) -> ImageManifest {
        disk_image(&[head_virtual_size]).to_manifest()
    }

    async fn retired_workdir(f: &Fixture) -> PathBuf {
        let workdir = f.tmp.path().join("retired");
        std::fs::create_dir(&workdir).unwrap();
        std::fs::write(workdir.join(DISK_OVERLAY_FILE), b"the job's disk").unwrap();
        AllocationRecord::new(
            digest(3),
            vec![ImageLocation {
                registry: "127.0.0.1:0".to_string(),
                repository: "treadmill/stub".to_string(),
            }],
            [(DISK_ROLE.to_string(), DISK_OVERLAY_FILE.to_string())],
        )
        .write(&workdir)
        .await
        .unwrap();
        workdir
    }

    /// Allocation records what a later resume needs to rebuild the chain: the
    /// image it was built on and where the overlay lives.
    #[tokio::test]
    async fn allocation_records_what_a_resume_needs() {
        let f = fixture(4 * GIB, vec![]);
        allocate(&f, 4 * GIB).await.unwrap();

        let record = AllocationRecord::read(f.tmp.path()).await.unwrap();
        assert_eq!(record.manifest_digest, digest(3));
        assert_eq!(
            record.overlays.get(DISK_ROLE).map(String::as_str),
            Some(DISK_OVERLAY_FILE),
        );
        assert_eq!(record.locations.len(), 1);
    }

    /// Adopting a retired job rebuilds the backing chain over the disk that is
    /// already there. Creating the overlay again is `qemu-img create`, which
    /// would silently discard everything the job wrote.
    #[tokio::test]
    async fn a_job_takes_an_image_lease_that_outlives_it() {
        let f = fixture_serving(4 * GIB, vec![], Some(resumable_manifest(4 * GIB)));
        let job_id = Uuid::new_v4();

        f.backend.fetch(&start_msg(job_id)).await.unwrap();

        assert_eq!(f.store.pinned(), vec![job_id.to_string()]);
        assert!(
            f.store.unpinned().is_empty(),
            "the lease is released when the working directory is collected, not here",
        );
    }

    #[tokio::test]
    async fn adopting_reuses_the_existing_overlay() {
        let f = fixture_serving(4 * GIB, vec![], Some(resumable_manifest(4 * GIB)));
        let workdir = retired_workdir(&f).await;
        let job_id = Uuid::new_v4();

        let mut vars = JobVars::new();
        let chain = f
            .backend
            .adopt(&start_msg(job_id), &workdir, &mut vars)
            .await
            .unwrap();

        assert_eq!(f.store.pinned(), vec![job_id.to_string()]);

        assert!(
            f.launcher.overlays().is_empty(),
            "a resume must not re-create the working disk",
        );
        assert_eq!(
            std::fs::read(workdir.join(DISK_OVERLAY_FILE)).unwrap(),
            b"the job's disk",
        );

        let args = chain.blockdev_args().join(" ");
        assert!(
            args.contains(&workdir.join(DISK_OVERLAY_FILE).display().to_string()),
            "{args}",
        );
        assert!(args.contains(&f.store.blob_path(&digest(1)).display().to_string()));

        assert_eq!(vars.get("disk_node").map(String::as_str), Some("tml-disk"),);
    }

    /// A retired directory this supervisor cannot make sense of is refused as
    /// unresumable, rather than booted against a disk of unknown provenance.
    #[tokio::test]
    async fn adopting_an_incomplete_retired_directory_cannot_resume() {
        let f = fixture_serving(4 * GIB, vec![], Some(resumable_manifest(4 * GIB)));

        let no_record = f.tmp.path().join("no-record");
        std::fs::create_dir(&no_record).unwrap();

        let no_overlay = retired_workdir(&f).await;
        std::fs::remove_file(no_overlay.join(DISK_OVERLAY_FILE)).unwrap();

        for workdir in [no_record, no_overlay] {
            let mut vars = JobVars::new();
            let error = f
                .backend
                .adopt(&start_msg(Uuid::new_v4()), &workdir, &mut vars)
                .await
                .unwrap_err();
            assert!(
                matches!(error.error_kind, connector::JobErrorKind::CannotResume),
                "{error:?}",
            );
        }
    }

    /// An image the coordinator dispatched in a shape this supervisor cannot
    /// boot is refused as incompatible rather than attempted.
    #[tokio::test]
    async fn a_non_image_specification_is_refused() {
        let f = fixture(4 * GIB, vec![]);

        let mut msg = start_msg(Uuid::new_v4());
        msg.image_spec = ImageSpecification::ResumeJob {
            job_id: Uuid::new_v4(),
        };

        let error = f.backend.fetch(&msg).await.unwrap_err();
        assert!(
            matches!(
                error.error_kind,
                connector::JobErrorKind::ImageNotCompatible
            ),
            "{error:?}",
        );
    }

    /// A manifest whose chains do not resolve is the image's fault.
    #[tokio::test]
    async fn a_manifest_with_a_dangling_lower_is_an_invalid_image() {
        let mut manifest = disk_image(&[GIB, GIB]).to_manifest();
        let mut layers = manifest.layers().clone();
        layers.remove(0);
        manifest.set_layers(layers);
        let f = fixture_serving(4 * GIB, vec![], Some(manifest));

        let error = f
            .backend
            .fetch(&start_msg(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::ImageInvalid),
            "{error:?}",
        );
    }

    /// An image built for another target, such as an nbd-netboot image, is
    /// refused rather than booted without the disks it expects.
    #[tokio::test]
    async fn an_image_without_exactly_a_disk_is_not_compatible() {
        let blob = |n| Blob {
            digest: digest(n),
            size: 10,
            format: BlobFormat::Qcow2 { virtual_size: GIB },
        };
        let mut netboot = ImageBuilder::default();
        netboot
            .push("rootfs".parse().unwrap(), blob(1))
            .unwrap()
            .push("bootfs".parse().unwrap(), blob(2))
            .unwrap();
        let mut extra = ImageBuilder::from_image(disk_image(&[GIB]));
        extra.push("efivars".parse().unwrap(), blob(2)).unwrap();

        for image in [netboot.build().unwrap(), extra.build().unwrap()] {
            let f = fixture_serving(4 * GIB, vec![], Some(image.to_manifest()));
            let error = f
                .backend
                .fetch(&start_msg(Uuid::new_v4()))
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error.error_kind,
                    connector::JobErrorKind::ImageNotCompatible
                ),
                "{error:?}",
            );
        }
    }

    /// A disk this supervisor cannot open as a block device is not compatible.
    #[tokio::test]
    async fn a_disk_of_an_unknown_format_is_not_compatible() {
        let manifest = disk_image(&[GIB]).to_manifest();
        // Canonical bytes, so the virtual size is followed by the role.
        let json = String::from_utf8(manifest_bytes(&manifest))
            .unwrap()
            .replace(
                "application/vnd.treadmill.qcow2",
                "application/vnd.example.future",
            )
            .replace(
                &format!(r#""dev.treadmill.qcow2.virtual-size":"{GIB}","#),
                "",
            );
        let f = fixture_serving(4 * GIB, vec![], Some(serde_json::from_str(&json).unwrap()));

        let image = f.backend.fetch(&start_msg(Uuid::new_v4())).await.unwrap();
        let error = f
            .backend
            .allocate(
                &start_msg(Uuid::new_v4()),
                f.tmp.path(),
                image,
                &mut JobVars::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error.error_kind,
                connector::JobErrorKind::ImageNotCompatible
            ),
            "{error:?}",
        );
        assert!(f.launcher.overlays().is_empty(), "nothing was allocated");
    }

    /// A manifest that is present but not a Treadmill image is the image's
    /// fault, not the store's.
    #[tokio::test]
    async fn a_manifest_that_is_not_a_treadmill_image_is_refused() {
        let json = r#"{
          "schemaVersion": 2,
          "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "config": { "mediaType": "application/vnd.oci.empty.v1+json",
                      "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                      "size": 2 },
          "layers": []
        }"#;
        let f = fixture_serving(4 * GIB, vec![], Some(serde_json::from_str(json).unwrap()));

        let error = f
            .backend
            .fetch(&start_msg(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::ImageInvalid),
            "{error:?}",
        );
    }
}
