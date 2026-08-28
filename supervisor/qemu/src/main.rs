use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{Level, event, instrument};

use treadmill_rs::api::switchboard_supervisor::ImageSpecification;
use treadmill_rs::connector::{self, StartJobMessage, SupervisorConnector};
use treadmill_rs::image::Digest;
use treadmill_rs::image::blockdev::BackingChain;
use treadmill_rs::image::parse::{self, ImageLayer, TreadmillImage};
use treadmill_rs::supervisor::{SupervisorBaseConfig, SupervisorCoordConnector};

use treadmill_supervisor_lib::capture::SerialSocket;
use treadmill_supervisor_lib::job::{JobBackend, JobRunner, JobRunnerConfig, JobVars, Workload};
use treadmill_supervisor_lib::launcher::{self, ProcessLauncher, StdioMode};
use treadmill_supervisor_lib::oci_store::{ImageStore, Location, OciStore, OciStoreConfig};
use treadmill_supervisor_lib::publisher::LogPublisherConfig;

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

    /// Directory to keep state:
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
    ///   chain ([`BackingChain::TOP_NODE`]). The supervisor prepends the
    ///   `-blockdev` nodes assembling the chain to the invocation, so the
    ///   configured args should attach the disk device by referencing this
    ///   node, e.g. `-device virtio-blk-device,drive={disk_node}`.
    ///
    /// - `tcp_control_socket_listen_addr`: full socket address, with an IPv6
    ///   address enclosed in square brackets, e.g. `[::1]:8080`
    qemu_args: Vec<String>,

    /// Maximum "working" disk image to be allocated for a job, in bytes.
    ///
    /// These are thinly provisioned qcow2 CoW files, and so don't necessarily
    /// take up this much space. However, all images will be extended to this
    /// size, and the virtual machine can then resize its internal partitions
    /// accordingly.
    ///
    /// The runner will be unable to execute any images that have a disk image
    /// with a size larger than this limit (even though the sparse qcow2 file
    /// may be smaller), as otherwise the image exposed to the VM may cut off a
    /// part of the image at the end.
    working_disk_max_bytes: u64,

    tcp_control_socket_listen_addr: std::net::SocketAddr,

    start_script: Option<PathBuf>,

    // TODO: add tests exercising the stop script, with failures at various
    // parts throughout the job lifecycle
    stop_script: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QemuSupervisorConfig {
    /// Base configuration, identical across all supervisors:
    base: SupervisorBaseConfig,

    /// Configurations for individual connector implementations. All are
    /// optional, and not all of them have to be supported:
    ws_connector: Option<treadmill_ws_connector::WsConnectorConfig>,

    /// Local OCI store (per-server Zot daemon) the supervisor pulls images from
    /// and reads blob files out of directly.
    oci_store: OciStoreConfig,

    /// Local tuning of the console capture→publish path. Optional: omitting
    /// the section leaves every field at its default.
    #[serde(default)]
    log_streaming: LogPublisherConfig,

    qemu: QemuConfig,
}

const COORD_MAILBOX_CAPACITY: usize = 8;

/// How long a connector that lost its coordinator waits before retrying.
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// The QEMU half of a job: the image it boots, the disk it boots from, and the
/// `qemu-system-*` process that boots it.
#[derive(Debug)]
pub struct QemuBackend {
    /// Read-only client of the local OCI store daemon (per-server Zot). We ask
    /// it to make a digest present, then open its on-disk blob files directly
    /// to assemble the backing chain. Injectable so the job state machine can
    /// be driven by tests with a stub store.
    image_store: Arc<dyn ImageStore>,

    /// Seam for the `qemu-img`/`qemu` subprocess operations, injectable so the
    /// job state machine can be driven by tests without spawning real binaries.
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

    /// Order the image's runtime backing chain base→head and map each layer to
    /// its on-disk blob path in the local store.
    ///
    /// The chain is read from the OCI manifest (D3): starting at the head, we
    /// follow each layer's `lower` annotation down to the base, guarding against
    /// dangling references and cycles. Returns the shared read-only lower paths
    /// **base-first** (ready for [`BackingChain::new`]) plus the head layer's
    /// advertised virtual size, used to size the per-job overlay. The backing
    /// paths are never baked into the shared blobs; the chain is assembled at
    /// launch via `-blockdev` nodes (D9).
    #[instrument(skip(self, image), err(Debug, level = Level::WARN))]
    fn assemble_backing_chain(&self, image: &TreadmillImage) -> Result<(Vec<PathBuf>, u64)> {
        let by_digest: HashMap<&Digest, &ImageLayer> =
            image.layers.iter().map(|l| (&l.digest, l)).collect();

        // Walk head → base following `lower`, collecting head-first.
        let mut head_first: Vec<&ImageLayer> = Vec::with_capacity(image.layers.len());
        let mut seen: std::collections::HashSet<&Digest> = std::collections::HashSet::new();
        let mut cursor = Some(&image.head);
        while let Some(digest) = cursor {
            let layer = by_digest
                .get(digest)
                .ok_or_else(|| anyhow!("backing chain references missing layer {digest}"))?;
            if !seen.insert(digest) {
                bail!("backing chain has a cycle at layer {digest}");
            }
            head_first.push(layer);
            cursor = layer.lower.as_ref();
        }

        let head_virtual_size = head_first
            .first()
            .ok_or_else(|| anyhow!("backing chain has no layers"))?
            .virtual_size
            .ok_or_else(|| anyhow!("head layer {} has no virtual-size annotation", image.head))?;

        // The shared read-only lowers, base-first, as direct store blob paths.
        let lower_paths = head_first
            .iter()
            .rev()
            .map(|layer| self.image_store.blob_path(&layer.digest))
            .collect();

        Ok((lower_paths, head_virtual_size))
    }
}

#[async_trait]
impl JobBackend for QemuBackend {
    type Image = TreadmillImage;
    type Allocation = BackingChain;

    /// Resolve the dispatched image into the local OCI store: ask it to make
    /// the manifest digest present — a copy from one of the dispatched
    /// locations, or a cache hit — then read+parse its manifest into the
    /// Treadmill backing-chain view.
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

    /// Assemble the runtime backing chain: the image's shared read-only lowers,
    /// base first, with a per-job writable overlay on top.
    ///
    /// The overlay is created with **no baked backing** (D3): the lower layers
    /// are supplied at launch as `-blockdev` nodes. It is sized to the
    /// configured working-disk maximum; the head's virtual size must fit within
    /// that ceiling.
    #[instrument(skip(self, _job, image, vars), err(Debug, level = Level::WARN))]
    async fn allocate(
        &self,
        _job: &StartJobMessage,
        workdir: &Path,
        image: TreadmillImage,
        vars: &mut JobVars,
    ) -> Result<BackingChain, connector::JobError> {
        // Order the OCI backing chain base→head and map each layer to its
        // read-only store blob path. The head's virtual size sizes the overlay.
        //
        // A malformed chain (dangling/cyclic lower, missing virtual size) is
        // treated as an invalid image.
        let (lower_paths, head_virtual_size) =
            self.assemble_backing_chain(&image)
                .map_err(|e| connector::JobError {
                    error_kind: connector::JobErrorKind::ImageInvalid,
                    description: format!("Invalid backing chain: {e:#}"),
                })?;

        // The per-job overlay backs onto the head at launch, so it must be at
        // least as large as the head's virtual size, and no larger than the
        // configured working-disk ceiling (the VM is exposed exactly this size).
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

        // Prepend the backing-chain `-blockdev` nodes (base → … → overlay) to
        // the configured invocation; the configured args attach the disk device
        // to the writable top node via the `{disk_node}` substitution.
        let mut qemu_args: Vec<String> = Vec::new();
        for node in chain.blockdev_args() {
            qemu_args.push("-blockdev".to_string());
            qemu_args.push(node);
        }
        qemu_args.extend(templated_args);

        // When the dispatch enabled log streaming, capture qemu's console
        // output: pipe stdout/stderr (read back by the runner) and route the
        // guest serial console to a unix socket we own. When it's disabled,
        // keep the historical behavior — stdout/stderr inherit our terminal and
        // the serial console goes wherever the configured args point it.
        let (stdio_mode, serial) = if job.log_streaming.is_some() {
            let serial_sock_path = workdir.join("serial.sock");
            match SerialSocket::bind(&serial_sock_path).await {
                Ok(socket) => {
                    // qemu connects to our already-bound listener as the client
                    // (`server=off`), so there is no connect race.
                    qemu_args.push("-chardev".to_string());
                    qemu_args.push(format!(
                        "socket,id=tml-serial,path={},server=off",
                        socket.path().display(),
                    ));
                    qemu_args.push("-serial".to_string());
                    qemu_args.push("chardev:tml-serial".to_string());
                    (StdioMode::Capture, Some(socket))
                }
                Err(e) => {
                    // Don't fail the job over a capture-setup error; fall back
                    // to inheriting and skip the serial channel.
                    event!(
                        Level::WARN,
                        ?serial_sock_path,
                        error = ?e,
                        "Failed to bind serial capture socket; disabling log capture for this job",
                    );
                    (StdioMode::Inherit, None)
                }
            }
        } else {
            (StdioMode::Inherit, None)
        };

        event!(
            Level::INFO,
            qemu_binary = ?self.config.qemu_binary,
            ?qemu_args,
            "Launching QEMU process",
        );
        let process = self
            .launcher
            .spawn(&self.config.qemu_binary, &qemu_args, None, stdio_mode)
            .await
            .map_err(|e| connector::JobError {
                error_kind: connector::JobErrorKind::InternalError,
                description: format!("Failed to launch the QEMU process: {e:#}"),
            })?;

        Ok(Workload { process, serial })
    }
}

impl QemuSupervisorConfig {
    fn job_runner(&self) -> JobRunnerConfig {
        JobRunnerConfig {
            supervisor_id: self.base.supervisor_id,
            job_address: self.base.job_address,
            state_dir: self.qemu.state_dir.clone(),
            control_socket_listen_addr: self.qemu.tcp_control_socket_listen_addr,
            start_script: self.qemu.start_script.clone(),
            stop_script: self.qemu.stop_script.clone(),
            log_streaming: self.log_streaming.clone(),
        }
    }
}

/// What to do when a connector's `run()` reports it lost its coordinator.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OnDisconnect {
    Reconnect,
    Exit,
}

/// Drive the runner off `connector` until it returns.
async fn serve(
    connector: Arc<dyn SupervisorConnector>,
    runner: Arc<JobRunner<QemuBackend>>,
    command_rx: mpsc::Receiver<connector::CoordCommand>,
    on_disconnect: OnDisconnect,
) {
    let commands = tokio::spawn({
        let runner = runner.clone();
        async move { runner.run(command_rx).await }
    });

    loop {
        match connector.run().await {
            Ok(()) => {
                event!(Level::INFO, "Connector exited, shutting down supervisor...");
                break;
            }
            Err(()) if on_disconnect == OnDisconnect::Reconnect => {
                event!(
                    Level::WARN,
                    "Connector exited with an error, reconnecting in {:?}...",
                    RECONNECT_DELAY,
                );
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(()) => {
                event!(Level::WARN, "Connector exited with an error.");
                break;
            }
        }
    }

    commands.abort();
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    event!(Level::INFO, "Treadmill Qemu Supervisor, Hello World!");

    let args = QemuSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file)
        .with_context(|| format!("Reading config file {:?}", args.config_file))?;
    let config: QemuSupervisorConfig = toml::from_str(&config_str)
        .with_context(|| format!("Parsing config file {:?}", args.config_file))?;

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.qemu.qemu_img_binary.clone(),
    ));

    let backend = Arc::new(QemuBackend::new(image_store, launcher, config.qemu.clone()));
    let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

    match config.base.coord_connector {
        SupervisorCoordConnector::WsConnector => {
            let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                "Requested WsConnector, but `ws_connector` config not present."
            ))?;

            let connector = Arc::new(treadmill_ws_connector::WsConnector::new(
                config.base.supervisor_id,
                ws_connector_config,
                command_tx,
            ));

            let runner = Arc::new(JobRunner::new(
                connector.clone(),
                backend,
                config.job_runner(),
            ));

            serve(connector, runner, command_rx, OnDisconnect::Reconnect).await;

            Ok(())
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

            let connector = Arc::new(treadmill_local_connector::LocalConnector::new(
                config.oci_store.registry.clone(),
                local_job,
                command_tx,
            ));

            let runner = Arc::new(JobRunner::new(
                connector.clone(),
                backend,
                config.job_runner(),
            ));

            // Ctrl-C requests a graceful shutdown: stop the job and let run()
            // return. A second Ctrl-C (after run() has returned) terminates the
            // process the usual way.
            let connector_for_signal = connector.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    event!(
                        Level::INFO,
                        "Received Ctrl-C => requesting graceful shutdown..."
                    );
                    connector_for_signal.request_shutdown();
                }
            });

            serve(connector, runner, command_rx, OnDisconnect::Exit).await;

            Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
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

    use treadmill_rs::api::switchboard_supervisor::{ImageLocation, ParameterValue, RestartPolicy};
    use treadmill_supervisor_lib::launcher::{QemuImgMetadata, WorkloadProcess};

    /// The shipped example is a deployment's starting point, so it has to
    /// parse as the configuration this supervisor actually reads.
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
        spawned: std::sync::Mutex<Vec<(PathBuf, Vec<String>)>>,
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
        async fn qcow2_info(&self, image: &Path) -> Result<QemuImgMetadata> {
            // Not exercised by the OCI path: the chain is read from the
            // manifest, not from qemu-img.
            Ok(QemuImgMetadata {
                filename: image.to_path_buf(),
                virtual_size: 0,
                children: vec![],
                encrypted: None,
                backing_filename_format: None,
                backing_filename: None,
                full_backing_filename: None,
            })
        }

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
            _stdio: StdioMode,
        ) -> Result<Box<dyn WorkloadProcess>> {
            self.spawned
                .lock()
                .unwrap()
                .push((program.to_path_buf(), args.to_vec()));
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

    /// The chain the manifest describes head→base is handed to QEMU base→head,
    /// which is the order the `-blockdev` nodes have to be emitted in, and the
    /// overlay is sized from the head's virtual size.
    #[tokio::test]
    async fn the_backing_chain_is_ordered_base_first() {
        let f = fixture(4 * GIB, vec![]);
        let (base, middle, head) = (digest(1), digest(2), digest(3));

        // Deliberately not in chain order in the manifest.
        let image = image(
            vec![
                over(base_layer(middle, Some(2 * GIB)), base),
                base_layer(base, Some(GIB)),
                over(base_layer(head, Some(4 * GIB)), middle),
            ],
            head,
        );

        let (paths, head_virtual_size) = f.backend.assemble_backing_chain(&image).unwrap();

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

    /// A layer whose `lower` names nothing in the manifest leaves the chain
    /// with no base, and cannot be assembled.
    #[tokio::test]
    async fn a_dangling_lower_is_refused() {
        let f = fixture(4 * GIB, vec![]);
        let head = digest(3);

        let image = image(vec![over(base_layer(head, Some(GIB)), digest(9))], head);

        let error = f
            .backend
            .assemble_backing_chain(&image)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing layer"), "{error}");
    }

    /// `lower` references that loop would walk forever; the walk stops at the
    /// layer it revisits.
    #[tokio::test]
    async fn a_cyclic_lower_is_refused() {
        let f = fixture(4 * GIB, vec![]);
        let (lower, head) = (digest(2), digest(3));

        let image = image(
            vec![
                over(base_layer(head, Some(GIB)), lower),
                over(base_layer(lower, Some(GIB)), head),
            ],
            head,
        );

        let error = f
            .backend
            .assemble_backing_chain(&image)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cycle"), "{error}");
    }

    /// Without the head's virtual size there is nothing to check the
    /// working-disk ceiling against, so the image is unusable even though the
    /// chain itself is well-formed.
    #[tokio::test]
    async fn a_head_without_a_virtual_size_is_refused() {
        let f = fixture(4 * GIB, vec![]);
        let head = digest(3);

        let image = image(vec![base_layer(head, None)], head);

        let error = f
            .backend
            .assemble_backing_chain(&image)
            .unwrap_err()
            .to_string();
        assert!(error.contains("virtual-size"), "{error}");
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

    /// The address the puppet's control socket is bound to is what the guest
    /// has to be pointed at, so the invocation can template it in rather than
    /// repeating the configured value.
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
