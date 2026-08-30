use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::signal::unix::SignalKind;
use tokio::sync::mpsc;
use tracing::{Level, event, instrument};

use treadmill_rs::api::switchboard_supervisor::{
    ImageSpecification, LogChannel, LogFormat, LogRender, LogView,
};
use treadmill_rs::connector::{self, StartJobMessage, SupervisorConnector};
use treadmill_rs::image::blockdev::BackingChain;
use treadmill_rs::image::parse::{self, ChainError, TreadmillImage};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_supervisor_lib::bootstrap::{self, COORD_MAILBOX_CAPACITY, OnDisconnect};
use treadmill_supervisor_lib::capture::SerialSocket;
use treadmill_supervisor_lib::job::{JobBackend, JobRunner, JobRunnerConfig, JobVars, Workload};
use treadmill_supervisor_lib::job_log::{self, JobLogRegistry};
use treadmill_supervisor_lib::launcher::{self, ProcessLauncher, StdioMode, WorkloadProcess};
use treadmill_supervisor_lib::oci_store::{ImageStore, Location, OciStore, OciStoreConfig};
use treadmill_supervisor_lib::publisher::LogPublisherConfig;
use treadmill_supervisor_lib::workdirs::{JobWorkdirs, RetentionConfig};

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
    ///   chain ([`BackingChain::TOP_NODE`]).
    ///
    ///   The supervisor internally prepends the `-blockdev` nodes assembling
    ///   the chain to the invocation, so the configured args should attach the
    ///   disk device by referencing this node, e.g. `-device
    ///   virtio-blk-device,drive={disk_node}`.
    ///
    /// - `tcp_control_socket_listen_addr`: the address the per-job control
    ///   socket is bound to, with an IPv6 address enclosed in square brackets,
    ///   e.g. `[::1]:8080`.
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

    tcp_control_socket_listen_addr: std::net::SocketAddr,

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

    /// Map the image's ordered backing chain to the blob paths in the local OCI
    /// store. Base first (ready for [`BackingChain::new`]), with the head
    /// layer's virtual size.
    fn chain_blob_paths(&self, image: &TreadmillImage) -> Result<(Vec<PathBuf>, u64), ChainError> {
        let (chain, head_virtual_size) = image.backing_chain()?;
        let paths = chain
            .into_iter()
            .map(|layer| self.image_store.blob_path(&layer.digest))
            .collect();
        Ok((paths, head_virtual_size))
    }
}

#[async_trait]
impl JobBackend for QemuBackend {
    type Image = TreadmillImage;
    type Allocation = BackingChain;

    async fn fetch(&self, job: &StartJobMessage) -> Result<TreadmillImage, connector::JobError> {
        let (manifest_digest, locations) = match &job.image_spec {
            ImageSpecification::Image {
                manifest_digest,
                locations,
            } => (
                *manifest_digest,
                locations
                    .iter()
                    .cloned()
                    .map(|loc| Location::new(loc.registry, loc.repository))
                    .collect::<Vec<_>>(),
            ),

            unsupported_image_spec => {
                return Err(connector::JobError {
                    error_kind: connector::JobErrorKind::ImageNotCompatible,
                    description: format!(
                        "Unsupported image specification: {unsupported_image_spec:?}",
                    ),
                });
            }
        };

        event!(
            Level::TRACE,
            %manifest_digest,
            ?locations,
            "Ensuring image present in the local OCI store",
        );

        self.image_store
            .ensure_present(&manifest_digest, &locations)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Failed to fetch image {manifest_digest}: {e:#}"),
            })?;

        let manifest = self
            .image_store
            .manifest(&manifest_digest)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Cannot retrieve image manifest of {manifest_digest}: {e:#}",),
            })?;

        parse::parse_image(&manifest).map_err(|e| connector::JobError {
            error_kind: connector::JobErrorKind::ImageInvalid,
            description: format!("Image {manifest_digest} is not a valid Treadmill image: {e}"),
        })
    }

    #[instrument(skip(self, _job, image, vars), err(Debug, level = Level::WARN))]
    async fn allocate(
        &self,
        _job: &StartJobMessage,
        workdir: &Path,
        image: TreadmillImage,
        vars: &mut JobVars,
    ) -> Result<BackingChain, connector::JobError> {
        // A malformed chain (dangling/cyclic lower, missing virtual size) is
        // treated as an invalid image.
        let (lower_paths, head_virtual_size) =
            self.chain_blob_paths(&image)
                .map_err(|e| connector::JobError {
                    error_kind: connector::JobErrorKind::ImageInvalid,
                    description: format!("Invalid backing chain: {e}"),
                })?;

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

        let overlay_file = workdir.join("overlay.qcow2");
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

        // The disk is attached by referencing the writable top node of the
        // backing chain the supervisor prepends as `-blockdev` args at launch.
        vars.insert("disk_node".to_string(), BackingChain::TOP_NODE.to_string());

        vars.insert(
            "tcp_control_socket_listen_addr".to_string(),
            self.config.tcp_control_socket_listen_addr.to_string(),
        );

        Ok(BackingChain::new(lower_paths, overlay_file))
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

        let process = self
            .spawn_qemu(chain, capture_args, templated_args, StdioMode::Capture)
            .await?;

        Ok(Workload { process, serial })
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
            channels: vec![LogChannel::Serial],
            order: 10,
            default: true,
            input: true,
        },
        LogView {
            id: "qemu".to_string(),
            label: "QEMU process".to_string(),
            render: LogRender::Text,
            format: LogFormat::Raw,
            channels: vec![LogChannel::QemuStdout, LogChannel::QemuStderr],
            order: 20,
            default: false,
            input: false,
        },
    ]
}

impl QemuSupervisorConfig {
    fn job_runner(&self, workdirs: Arc<JobWorkdirs>, job_log: JobLogRegistry) -> JobRunnerConfig {
        JobRunnerConfig {
            supervisor_id: self.base.supervisor_id,
            job_address: self.base.job_address,
            workdirs,
            control_socket_listen_addr: self.qemu.tcp_control_socket_listen_addr,
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

    let backend = Arc::new(QemuBackend::new(image_store, launcher, config.qemu.clone()));
    let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

    // SIGHUP lets the switchboard finish with the job it dispatched. This
    // allows for the supervisor to be gracefully updated after finishing a job,
    // without interrupting it.
    //
    // A one-shot local job there is nobody to wait for and it is terminated &
    // removed when getting a SIGINT / Ctrl-C instead.
    let (connector, drain_signal, on_disconnect): (Arc<dyn SupervisorConnector>, _, _) =
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
                    SignalKind::hangup(),
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
                    SignalKind::interrupt(),
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

    bootstrap::serve(connector, runner, command_rx, drain_signal, on_disconnect).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Direct drive of the QEMU backend: the backing chain it reads off an
    //! image, the overlay it sizes against the working-disk ceiling, and the
    //! invocation it hands to QEMU. The lifecycle these steps hang off is the
    //! runner's, and is tested in `treadmill_supervisor_lib::job`.

    use super::*;

    use std::collections::HashMap;
    use std::process::ExitStatus;

    use oci_spec::image::ImageManifest;
    use tempfile::TempDir;
    use uuid::Uuid;

    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, LogStreamingDispatch, ParameterValue, RestartPolicy,
    };
    use treadmill_rs::image::Digest;
    use treadmill_rs::image::parse::ImageLayer;
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
        });
        let launcher = Arc::new(StubLauncher::default());

        let config = QemuConfig {
            qemu_binary: PathBuf::from("/nonexistent/qemu"),
            qemu_img_binary: PathBuf::from("/nonexistent/qemu-img"),
            state_dir: tmp.path().join("state"),
            qemu_args: qemu_args.into_iter().map(str::to_string).collect(),
            working_disk_max_bytes,
            tcp_control_socket_listen_addr: "127.0.0.1:3859".parse().unwrap(),
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

    /// A layer with no `lower`, i.e. the base of a chain.
    fn base_layer(d: Digest, virtual_size: Option<u64>) -> ImageLayer {
        ImageLayer {
            digest: d,
            size: 10,
            media_type: "application/vnd.treadmill.disk.qcow2".to_string(),
            role: None,
            virtual_size,
            lower: None,
        }
    }

    fn over(mut layer: ImageLayer, lower: Digest) -> ImageLayer {
        layer.lower = Some(lower);
        layer
    }

    fn image(layers: Vec<ImageLayer>, head: Digest) -> TreadmillImage {
        TreadmillImage {
            layers,
            head,
            title: None,
            version: None,
            description: None,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The ordered chain is mapped to the blob paths the local store holds the
    /// layers at, in the order the `-blockdev` nodes have to be emitted in, and
    /// the overlay is sized from the head's virtual size. (The ordering itself
    /// is the image's business, and is tested in `treadmill_rs::image::parse`.)
    #[tokio::test]
    async fn the_chain_is_mapped_to_store_blob_paths() {
        let f = fixture(4 * GIB, vec![]);
        let (base, middle, head) = (digest(1), digest(2), digest(3));

        let image = image(
            vec![
                over(base_layer(middle, Some(2 * GIB)), base),
                base_layer(base, Some(GIB)),
                over(base_layer(head, Some(4 * GIB)), middle),
            ],
            head,
        );

        let (paths, head_virtual_size) = f.backend.chain_blob_paths(&image).unwrap();

        assert_eq!(
            paths,
            vec![
                f.store.blob_path(&base),
                f.store.blob_path(&middle),
                f.store.blob_path(&head),
            ],
        );
        assert_eq!(head_virtual_size, 4 * GIB);
    }

    /// A chain the image cannot assemble fails the job as an invalid image,
    /// whatever is wrong with it.
    #[tokio::test]
    async fn a_malformed_chain_is_an_invalid_image() {
        let f = fixture(4 * GIB, vec![]);
        let head = digest(3);

        // A `lower` naming nothing in the manifest: no base to stack on.
        let image = image(vec![over(base_layer(head, Some(GIB)), digest(9))], head);

        let mut vars = JobVars::new();
        let error = f
            .backend
            .allocate(&start_msg(Uuid::new_v4()), f.tmp.path(), image, &mut vars)
            .await
            .unwrap_err();

        assert!(
            matches!(error.error_kind, connector::JobErrorKind::ImageInvalid),
            "{error:?}",
        );
        assert!(f.launcher.overlays().is_empty(), "nothing was allocated");
    }

    async fn allocate(
        f: &Fixture,
        head_virtual_size: u64,
    ) -> Result<BackingChain, connector::JobError> {
        let head = digest(3);
        let image = image(vec![base_layer(head, Some(head_virtual_size))], head);
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
            parameters: HashMap::<String, ParameterValue>::new(),
            log_streaming: None,
            gateway: None,
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
        let head = digest(3);
        let image = image(vec![base_layer(head, Some(GIB))], head);
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
                &format!("virtio-blk-pci,drive={}", BackingChain::TOP_NODE),
            ],
        );
    }

    /// The address the control socket is bound to is available to the
    /// invocation, so a configuration that can use it verbatim -- one bound to
    /// an address the guest can reach -- need not repeat the value.
    #[tokio::test]
    async fn the_control_socket_address_is_available_to_the_invocation() {
        let f = fixture(
            4 * GIB,
            vec![
                "-fw_cfg",
                "name=opt/org.tockos.treadmill.tcp-ctrl-socket,string={tcp_control_socket_listen_addr}",
            ],
        );

        let job_id = Uuid::new_v4();
        let mut vars = runner_vars(job_id, f.tmp.path());
        let head = digest(3);
        let image = image(vec![base_layer(head, Some(GIB))], head);
        let chain = f
            .backend
            .allocate(&start_msg(job_id), f.tmp.path(), image, &mut vars)
            .await
            .unwrap();

        f.backend
            .launch(&start_msg(job_id), f.tmp.path(), chain, &vars)
            .await
            .unwrap();

        assert!(
            f.launcher.spawned_args().contains(
                &"name=opt/org.tockos.treadmill.tcp-ctrl-socket,string=127.0.0.1:3859".to_string()
            ),
            "{:?}",
            f.launcher.spawned_args(),
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
        let head = digest(3);
        let image = image(vec![base_layer(head, Some(GIB))], head);

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
        let head = digest(3);
        let image = image(vec![base_layer(head, Some(GIB))], head);

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
