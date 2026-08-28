use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{Level, event, info, instrument, warn};

use treadmill_rs::api::switchboard_supervisor::ImageSpecification;
use treadmill_rs::connector::{self, StartJobMessage};
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

#[tokio::main]
async fn main() -> Result<()> {
    use treadmill_rs::connector::SupervisorConnector;

    tracing_subscriber::fmt::init();
    event!(Level::INFO, "Treadmill Qemu Supervisor, Hello World!");

    let args = QemuSupervisorArgs::parse();

    let config_str = std::fs::read_to_string(&args.config_file).unwrap();
    let config: QemuSupervisorConfig = toml::from_str(&config_str).unwrap();

    let image_store: Arc<dyn ImageStore> = Arc::new(OciStore::new(
        config.oci_store.registry.clone(),
        config.oci_store.store_root.clone(),
    ));

    let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher::CliLauncher::new(
        config.qemu.qemu_img_binary.clone(),
    ));

    let backend = Arc::new(QemuBackend::new(image_store, launcher, config.qemu.clone()));
    let runner_config = config.job_runner();

    match config.base.coord_connector {
        SupervisorCoordConnector::WsConnector => {
            let ws_connector_config = config.ws_connector.clone().ok_or(anyhow!(
                "Requested WsConnector, but `ws_connector` config not present."
            ))?;

            let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

            let connector = Arc::new(treadmill_ws_connector::WsConnector::new(
                config.base.supervisor_id,
                ws_connector_config,
                command_tx,
            ));

            let runner = Arc::new(JobRunner::new(connector.clone(), backend, runner_config));
            let commands = tokio::spawn(async move { runner.run(command_rx).await });

            loop {
                if let Err(()) = connector.run().await {
                    warn!("Run method exited with error, trying to reconnect in 1 second...");
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                } else {
                    info!("Run method exited, shutting down supervisor...");
                    break;
                }
            }

            commands.abort();

            Ok(())
        }
        SupervisorCoordConnector::Local => {
            // One-shot, switchboard-less run: drive a single job from the
            // command-line `LocalJobArgs` against the local OCI store.
            let local_job = args.local_job.clone().ok_or(anyhow!(
                "Requested the `local` connector, but no job was supplied on the \
                 command line (need at least --manifest-digest and --repository)."
            ))?;
            if local_job.manifest_digest.is_none() || local_job.repository.is_none() {
                bail!("The `local` connector requires both --manifest-digest and --repository.");
            }
            let registry = config.oci_store.registry.clone();

            let (command_tx, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);

            let connector = Arc::new(treadmill_local_connector::LocalConnector::new(
                registry, local_job, command_tx,
            ));

            let runner = Arc::new(JobRunner::new(connector.clone(), backend, runner_config));
            let commands = tokio::spawn(async move { runner.run(command_rx).await });

            // Ctrl-C requests a graceful shutdown: stop the job and let run()
            // return. A second Ctrl-C (after run() has returned) terminates the
            // process the usual way.
            let connector_for_signal = connector.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("Received Ctrl-C => requesting graceful shutdown...");
                    connector_for_signal.request_shutdown();
                }
            });

            if let Err(()) = connector.run().await {
                warn!("Local run exited with an error.");
            } else {
                info!("Local run finished, shutting down supervisor...");
            }

            commands.abort();

            Ok(())
        }
        unsupported_connector => {
            bail!("Unsupported coord connector: {:?}", unsupported_connector);
        }
    }
}

#[cfg(test)]
mod tests {
    //! In-process drive of the job state machine (plan §12, Phase 0.5).
    //!
    //! With the image store, subprocess launcher, and connector all behind
    //! traits, we can drive `start_job → Ready → Terminated` against stubs —
    //! asserting the reported state transitions and that the workload would have
    //! been launched — without spawning a single real binary.

    use super::*;

    use std::process::ExitStatus;

    use tokio::sync::{oneshot, watch};
    use uuid::Uuid;

    use treadmill_rs::api;
    use treadmill_rs::api::switchboard_supervisor::{
        ImageLocation, JobGatewayDispatch, JobInitializingStage, JobService, ParameterValue,
        ReportedSupervisorStatus, RestartPolicy, RunningJobState, SupervisorEvent,
        SupervisorJobEvent,
    };
    use treadmill_rs::connector::SupervisorConnector;
    use treadmill_rs::control_socket::Supervisor as _;

    use oci_spec::image::ImageManifest;
    use treadmill_supervisor_lib::job::{JobControlEndpoint, JobFacts, Phase};
    use treadmill_supervisor_lib::launcher::{QemuImgMetadata, WorkloadProcess};

    /// Connector that records the job state transitions and errors reported to
    /// it.
    #[derive(Debug, Default)]
    struct RecordingConnector {
        states: std::sync::Mutex<Vec<RunningJobState>>,
        errors: std::sync::Mutex<Vec<connector::JobError>>,
        addresses: std::sync::Mutex<Vec<std::net::IpAddr>>,
        service_sets: std::sync::Mutex<Vec<Vec<JobService>>>,
    }

    impl RecordingConnector {
        fn labels(&self) -> Vec<String> {
            self.states.lock().unwrap().iter().map(label).collect()
        }

        fn errors(&self) -> Vec<connector::JobError> {
            self.errors.lock().unwrap().clone()
        }

        fn addresses(&self) -> Vec<std::net::IpAddr> {
            self.addresses.lock().unwrap().clone()
        }

        fn service_sets(&self) -> Vec<Vec<JobService>> {
            self.service_sets.lock().unwrap().clone()
        }
    }

    fn label(s: &RunningJobState) -> String {
        match s {
            RunningJobState::Initializing { stage } => {
                let stage = match stage {
                    JobInitializingStage::Starting => "starting",
                    JobInitializingStage::FetchingImage => "fetching_image",
                    JobInitializingStage::Allocating => "allocating",
                    JobInitializingStage::Provisioning => "provisioning",
                    JobInitializingStage::Booting => "booting",
                };
                format!("initializing/{stage}")
            }
            RunningJobState::Ready => "ready".to_string(),
            RunningJobState::Terminating => "terminating".to_string(),
            RunningJobState::Terminated => "terminated".to_string(),
        }
    }

    #[async_trait]
    impl SupervisorConnector for RecordingConnector {
        async fn run(&self) -> Result<(), ()> {
            Ok(())
        }

        async fn emit(&self, event: SupervisorEvent) {
            let SupervisorEvent::JobEvent { event, .. } = event;
            match event {
                SupervisorJobEvent::StateTransition { new_state, .. } => {
                    self.states.lock().unwrap().push(new_state);
                }
                SupervisorJobEvent::Error { error } => {
                    self.errors.lock().unwrap().push(error);
                }
                SupervisorJobEvent::JobNetworkAddress { address } => {
                    self.addresses.lock().unwrap().push(address);
                }
                SupervisorJobEvent::JobServiceSet { services } => {
                    self.service_sets.lock().unwrap().push(services);
                }
                _ => {}
            }
        }
    }

    /// OCI store stub: returns a canned manifest + a fixed blob path for any
    /// digest, simulating a present image.
    #[derive(Debug)]
    struct StubStore {
        blob_file: PathBuf,
        manifest: ImageManifest,
    }

    #[async_trait]
    impl ImageStore for StubStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<()> {
            Ok(())
        }
        async fn manifest(&self, _: &Digest) -> Result<ImageManifest> {
            Ok(self.manifest.clone())
        }
        fn blob_path(&self, _: &Digest) -> PathBuf {
            self.blob_file.clone()
        }
    }

    /// Launcher that records what it was asked to spawn (instead of spawning
    /// anything) and no-ops the qcow2 operations.
    #[derive(Debug, Default)]
    struct StubLauncher {
        spawned: std::sync::Mutex<Vec<(PathBuf, Vec<String>)>>,
    }

    #[async_trait]
    impl ProcessLauncher for StubLauncher {
        async fn qcow2_info(&self, image: &Path) -> Result<QemuImgMetadata> {
            // Not exercised by the OCI path (the chain is read from the
            // manifest, not qemu-img); return a benign record.
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

        async fn create_overlay_no_backing(&self, _: &Path, _: u64) -> Result<()> {
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

    /// A workload that never exits on its own — it only ends when killed, which
    /// is exactly the path `terminate_job` drives.
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

    // Canned single-layer OCI manifest digests (any valid sha256 strings).
    const ROOT_DIGEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const EMPTY_DIGEST: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    /// A minimal single-layer Treadmill OCI image manifest the stub serves, with
    /// the head layer advertising `virtual_size`.
    fn single_layer_manifest(virtual_size: u64) -> ImageManifest {
        let json = format!(
            r#"{{
              "schemaVersion": 2,
              "mediaType": "application/vnd.oci.image.manifest.v1+json",
              "artifactType": "application/vnd.treadmill.image.v1+json",
              "config": {{ "mediaType": "application/vnd.oci.empty.v1+json", "digest": "{EMPTY_DIGEST}", "size": 2 }},
              "layers": [
                {{ "mediaType": "application/vnd.treadmill.disk.qcow2", "digest": "{ROOT_DIGEST}", "size": 10,
                   "annotations": {{ "dev.treadmill.role": "root", "dev.treadmill.qcow2.virtual-size": "{virtual_size}" }} }}
              ],
              "annotations": {{ "dev.treadmill.qcow2.head": "{ROOT_DIGEST}" }}
            }}"#
        );
        serde_json::from_str(&json).expect("canned manifest parses as an OCI image manifest")
    }

    async fn endpoint(sup: &Runner) -> JobControlEndpoint {
        JobControlEndpoint::new(sup.job().await.expect("a job occupies the slot"))
    }

    async fn job_facts(sup: &Runner) -> watch::Receiver<Arc<JobFacts>> {
        sup.job()
            .await
            .expect("a job occupies the slot")
            .facts_watch()
    }

    async fn idle(sup: &Runner) -> bool {
        matches!(sup.status().await, ReportedSupervisorStatus::Idle)
    }

    async fn wait_for(
        facts: &mut watch::Receiver<Arc<JobFacts>>,
        reached: impl Fn(&JobFacts) -> bool,
    ) {
        loop {
            if reached(&facts.borrow_and_update()) {
                return;
            }
            facts
                .changed()
                .await
                .expect("the job task keeps publishing its facts");
        }
    }

    fn booting(facts: &JobFacts) -> bool {
        matches!(facts.phase, Phase::Booting)
    }

    fn ready(facts: &JobFacts) -> bool {
        matches!(facts.phase, Phase::Ready)
    }

    fn terminated(facts: &JobFacts) -> bool {
        facts.phase.terminated()
    }

    async fn start_and_boot(h: &Harness, msg: StartJobMessage) -> watch::Receiver<Arc<JobFacts>> {
        let job_id = msg.job_id;
        h.sup.start_job(msg).await.unwrap();

        let mut facts = job_facts(&h.sup).await;
        wait_for(&mut facts, booting).await;

        endpoint(&h.sup)
            .await
            .puppet_ready(0, Uuid::new_v4(), job_id)
            .await;
        wait_for(&mut facts, ready).await;

        facts
    }

    type Runner = Arc<JobRunner<QemuBackend>>;

    /// A constructed supervisor plus the stubs wired into it, over a temp dir.
    struct Harness {
        sup: Runner,
        connector: Arc<RecordingConnector>,
        launcher: Arc<StubLauncher>,
        tmp: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    /// Build a supervisor whose stub image's head layer declares
    /// `head_virtual_size`, against a working-disk ceiling of
    /// `working_disk_max_bytes` — equal for a valid image, with the head larger
    /// than the ceiling to provoke an `ImageInvalid` failure. Configures no
    /// job address, as a deployment without a gateway has none.
    fn harness(head_virtual_size: u64, working_disk_max_bytes: u64) -> Harness {
        harness_with(head_virtual_size, working_disk_max_bytes, None, false, None)
    }

    /// Like [`harness`], for a supervisor reading from `store` instead of the
    /// always-present [`StubStore`].
    fn harness_with_store(head_virtual_size: u64, store: Arc<dyn ImageStore>) -> Harness {
        harness_with(
            head_virtual_size,
            head_virtual_size,
            None,
            false,
            Some(store),
        )
    }

    /// Like [`harness`], for a supervisor configured to report `job_address`
    /// for the jobs it runs.
    fn harness_with_job_address(
        head_virtual_size: u64,
        working_disk_max_bytes: u64,
        job_address: Option<std::net::IpAddr>,
    ) -> Harness {
        harness_with(
            head_virtual_size,
            working_disk_max_bytes,
            job_address,
            false,
            None,
        )
    }

    /// Like [`harness`], for a supervisor configured with start and stop
    /// scripts that each append a line to `<tmp>/<start|stop>-hook.log`, so a
    /// test can count how often they ran (see [`hook_runs`]).
    fn harness_with_hooks(head_virtual_size: u64, working_disk_max_bytes: u64) -> Harness {
        harness_with(head_virtual_size, working_disk_max_bytes, None, true, None)
    }

    /// Count the lines the named hook appended, zero if it never ran.
    fn hook_runs(tmp: &Path, hook: &str) -> usize {
        std::fs::read_to_string(tmp.join(format!("{hook}-hook.log")))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    /// Write a hook script appending one line to `<tmp>/<hook>-hook.log`.
    fn write_hook(tmp: &Path, hook: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = tmp.join(format!("{hook}-hook.sh"));
        let log = tmp.join(format!("{hook}-hook.log"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho ran >> {}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn harness_with(
        head_virtual_size: u64,
        working_disk_max_bytes: u64,
        job_address: Option<std::net::IpAddr>,
        hooks: bool,
        store: Option<Arc<dyn ImageStore>>,
    ) -> Harness {
        let tmp = std::env::temp_dir().join(format!("tml-qemu-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let blob_file = tmp.join("root.qcow2");
        std::fs::write(&blob_file, b"not-a-real-qcow2").unwrap();

        let connector = Arc::new(RecordingConnector::default());
        let store: Arc<dyn ImageStore> = store.unwrap_or_else(|| {
            Arc::new(StubStore {
                blob_file,
                manifest: single_layer_manifest(head_virtual_size),
            })
        });
        let launcher = Arc::new(StubLauncher::default());

        let config = QemuSupervisorConfig {
            base: SupervisorBaseConfig {
                coord_connector: SupervisorCoordConnector::WsConnector,
                supervisor_id: Uuid::new_v4(),
                job_address,
            },
            ws_connector: None,
            oci_store: OciStoreConfig {
                registry: "127.0.0.1:0".to_string(),
                store_root: tmp.clone(),
            },
            log_streaming: LogPublisherConfig::default(),
            qemu: QemuConfig {
                qemu_binary: PathBuf::from("/nonexistent/qemu"),
                qemu_img_binary: PathBuf::from("/nonexistent/qemu-img"),
                state_dir: tmp.join("state"),
                qemu_args: vec![],
                working_disk_max_bytes,
                tcp_control_socket_listen_addr: "127.0.0.1:0".parse().unwrap(),
                start_script: hooks.then(|| write_hook(&tmp, "start")),
                stop_script: hooks.then(|| write_hook(&tmp, "stop")),
            },
        };
        let sup = Arc::new(JobRunner::new(
            connector.clone(),
            Arc::new(QemuBackend::new(
                store,
                launcher.clone(),
                config.qemu.clone(),
            )),
            config.job_runner(),
        ));

        Harness {
            sup,
            connector,
            launcher,
            tmp,
        }
    }

    fn start_msg(job_id: Uuid) -> StartJobMessage {
        start_msg_with_gateway(job_id, None)
    }

    /// Like [`start_msg`], for a job the coordinator dispatched with gateway
    /// material for the supervisor to relay into it.
    fn start_msg_with_gateway(
        job_id: Uuid,
        gateway: Option<JobGatewayDispatch>,
    ) -> StartJobMessage {
        StartJobMessage {
            job_id,
            image_spec: ImageSpecification::Image {
                manifest_digest: ROOT_DIGEST.parse().unwrap(),
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
            gateway,
        }
    }

    /// An [`ImageStore`] whose `ensure_present` blocks until the test releases
    /// it, so a job can be stopped while it is still fetching its image.
    #[derive(Debug)]
    struct GatedStore {
        inner: StubStore,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ImageStore for GatedStore {
        async fn ensure_present(&self, _: &Digest, _: &[Location]) -> Result<()> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
        async fn manifest(&self, digest: &Digest) -> Result<ImageManifest> {
            self.inner.manifest(digest).await
        }
        fn blob_path(&self, digest: &Digest) -> PathBuf {
            self.inner.blob_path(digest)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_lifecycle_transitions() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);

        let job_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();

        h.sup.start_job(start_msg(job_id)).await.unwrap();
        let mut facts = job_facts(&h.sup).await;
        wait_for(&mut facts, booting).await;

        assert_eq!(
            h.connector.labels(),
            vec![
                "initializing/starting",
                "initializing/fetching_image",
                "initializing/allocating",
                "initializing/booting",
            ],
        );

        // The workload was launched with the configured QEMU binary.
        {
            let spawned = h.launcher.spawned.lock().unwrap();
            assert_eq!(spawned.len(), 1);
            assert_eq!(spawned[0].0, PathBuf::from("/nonexistent/qemu"));
        }

        // Puppet reports ready → the job goes Ready.
        endpoint(&h.sup)
            .await
            .puppet_ready(0, host_id, job_id)
            .await;
        wait_for(&mut facts, ready).await;
        assert_eq!(
            h.connector.labels().last().map(String::as_str),
            Some("ready")
        );

        // Terminating kills the (stub) workload and reports the terminal
        // transition before it returns.
        h.sup.terminate_job(job_id).await.unwrap();

        let labels = h.connector.labels();
        assert!(labels.iter().any(|l| l == "terminating"), "{labels:?}");
        assert_eq!(labels.last().map(String::as_str), Some("terminated"));
        assert!(h.connector.errors().is_empty());

        // The record is retained until it is removed.
        assert!(terminated(&facts.borrow_and_update()));
        h.sup.remove_job(job_id).await.unwrap();
        assert!(idle(&h.sup).await);
    }

    /// A supervisor configured with a job address reports it as the job starts,
    /// so the coordinator has somewhere to point a gateway at before the job is
    /// up. One without stays silent, and the job is reachable from nowhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_configured_job_address_is_reported_at_start() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let address: std::net::IpAddr = "fd00::2".parse().unwrap();
        let h = harness_with_job_address(virtual_size, virtual_size, Some(address));

        assert!(h.connector.addresses().is_empty(), "nothing has started");

        start_and_boot(&h, start_msg(Uuid::new_v4())).await;
        assert_eq!(h.connector.addresses(), vec![address]);

        let unconfigured = harness(virtual_size, virtual_size);
        start_and_boot(&unconfigured, start_msg(Uuid::new_v4())).await;
        assert!(unconfigured.connector.addresses().is_empty());
    }

    /// The puppet asks its supervisor what to validate service tokens against,
    /// and gets back exactly what the coordinator dispatched the job with —
    /// nothing if the job was dispatched without a gateway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_job_is_told_its_gateway_material() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let host_id = Uuid::new_v4();
        let dispatched = JobGatewayDispatch {
            issuer: "https://switchboard.example".to_string(),
            signing_public_key: "-----BEGIN PUBLIC KEY-----\nstub\n-----END PUBLIC KEY-----\n"
                .to_string(),
            key_id: "wI9c-yvsF8".to_string(),
            endpoints: vec![
                api::switchboard_supervisor::JobGatewayEndpoint {
                    base_domain: "gw-us-east-1.treadmillusercontent.com".to_string(),
                    port: 443,
                },
                api::switchboard_supervisor::JobGatewayEndpoint {
                    base_domain: "gw-eu-central-1.treadmillusercontent.com".to_string(),
                    port: 4433,
                },
            ],
        };

        let h = harness(virtual_size, virtual_size);
        let job_id = Uuid::new_v4();

        start_and_boot(&h, start_msg_with_gateway(job_id, Some(dispatched.clone()))).await;
        let puppet = endpoint(&h.sup).await;

        // Nothing is answered for a job this endpoint does not serve.
        assert!(puppet.gateway(host_id, Uuid::new_v4()).await.is_none());

        let relayed = puppet
            .gateway(host_id, job_id)
            .await
            .expect("a job dispatched with a gateway is told about it");
        assert_eq!(relayed.issuer, dispatched.issuer);
        assert_eq!(relayed.signing_public_key, dispatched.signing_public_key);
        assert_eq!(relayed.key_id, dispatched.key_id);
        assert_eq!(
            relayed.endpoints,
            dispatched
                .endpoints
                .iter()
                .cloned()
                .map(
                    |api::switchboard_supervisor::JobGatewayEndpoint { base_domain, port }| {
                        api::supervisor_puppet::JobGatewayEndpoint { base_domain, port }
                    }
                )
                .collect::<Vec<_>>()
        );

        // A job dispatched without one has none to be told about.
        let plain = harness(virtual_size, virtual_size);
        let plain_job = Uuid::new_v4();
        start_and_boot(&plain, start_msg(plain_job)).await;
        assert!(
            endpoint(&plain.sup)
                .await
                .gateway(host_id, plain_job)
                .await
                .is_none()
        );
    }

    /// A service announcement is relayed to the coordinator as it arrives: the
    /// supervisor stores nothing and interprets nothing. An announcement
    /// carries a job's whole set, so a later one replaces the earlier rather
    /// than adding to it — including an empty one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_announced_service_set_is_relayed_to_the_coordinator() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);

        let job_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();
        start_and_boot(&h, start_msg(job_id)).await;
        assert!(h.connector.service_sets().is_empty());

        let announced = vec![
            JobService {
                name: "webide".to_string(),
                label: Some("Web IDE".to_string()),
                protocol: "webapp".to_string(),
            },
            JobService {
                name: "shell".to_string(),
                label: None,
                protocol: "sshws".to_string(),
            },
        ];
        let puppet = endpoint(&h.sup).await;
        puppet
            .job_service_set(0, announced.clone(), host_id, job_id)
            .await;
        puppet.job_service_set(1, Vec::new(), host_id, job_id).await;

        // Puppet events are ordered against the job's state changes, so a
        // terminate the job has acted on proves both were relayed first.
        h.sup.terminate_job(job_id).await.unwrap();
        assert_eq!(h.connector.service_sets(), vec![announced, Vec::new()]);
    }

    /// A job that fails on its way up still owes the coordinator a terminal
    /// transition (D2.2): the reported error is the *cause* of the
    /// termination, never a substitute for it. Its record is then retained,
    /// occupying the slot, until it is removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_startup_failure_reports_the_error_and_then_terminated() {
        // The image head's virtual size exceeds the working-disk ceiling, so the
        // job fails as ImageInvalid before any workload is launched.
        let h = harness(8 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);

        let job_id = Uuid::new_v4();

        h.sup.start_job(start_msg(job_id)).await.unwrap();
        let mut facts = job_facts(&h.sup).await;
        wait_for(&mut facts, terminated).await;

        let errors = h.connector.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0].error_kind, connector::JobErrorKind::ImageInvalid),
            "{:?}",
            errors[0],
        );

        // Validation failed before launch, and the job never reached
        // Booting/Ready.
        assert!(h.launcher.spawned.lock().unwrap().is_empty());
        let labels = h.connector.labels();
        assert!(
            !labels
                .iter()
                .any(|l| l == "initializing/booting" || l == "ready"),
            "{labels:?}",
        );

        // It did reach Terminated, exactly once.
        assert_eq!(labels.last().map(String::as_str), Some("terminated"));
        assert_eq!(labels.iter().filter(|l| *l == "terminated").count(), 1);

        // The failed job is retained: it still holds the slot until removed.
        let error = h
            .sup
            .start_job(start_msg(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::MaxConcurrentJobs),
            "{error:?}",
        );

        h.sup.remove_job(job_id).await.unwrap();
        assert!(idle(&h.sup).await);
    }

    /// D2.3/D2.4: the coordinator may repeat either command, or send one for a
    /// job this supervisor never heard of. A postcondition that already holds
    /// is not an error, and no repeat produces a second terminal transition.
    /// Only removing a job that is still executing is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminate_and_remove_are_idempotent() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);
        let job_id = Uuid::new_v4();

        // Nothing is known about this job, so both commands are satisfied.
        h.sup.terminate_job(job_id).await.unwrap();
        h.sup.remove_job(job_id).await.unwrap();

        let mut facts = start_and_boot(&h, start_msg(job_id)).await;

        // A live job must be terminated before it can be removed.
        let error = h.sup.remove_job(job_id).await.unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::NotTerminated),
            "{error:?}",
        );

        h.sup.terminate_job(job_id).await.unwrap();
        assert!(terminated(&facts.borrow_and_update()));

        h.sup.terminate_job(job_id).await.unwrap();
        h.sup.remove_job(job_id).await.unwrap();
        h.sup.remove_job(job_id).await.unwrap();
        assert!(idle(&h.sup).await);

        let labels = h.connector.labels();
        assert_eq!(
            labels.iter().filter(|l| *l == "terminating").count(),
            1,
            "{labels:?}",
        );
        assert_eq!(
            labels.iter().filter(|l| *l == "terminated").count(),
            1,
            "{labels:?}",
        );
        assert!(
            h.connector.errors().is_empty(),
            "{:?}",
            h.connector.errors()
        );
    }

    /// D2.1: this supervisor runs a single job, which occupies its slot from
    /// `StartJob` until `RemoveJob` — a terminated-but-retained job refuses a
    /// new one just as a live one does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_job_is_refused_while_one_occupies_the_slot() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);
        let occupant = Uuid::new_v4();
        let next = Uuid::new_v4();

        start_and_boot(&h, start_msg(occupant)).await;

        let error = h.sup.start_job(start_msg(occupant)).await.unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::JobAlreadyExists),
            "{error:?}",
        );

        let error = h.sup.start_job(start_msg(next)).await.unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::AlreadyRunning),
            "{error:?}",
        );

        h.sup.terminate_job(occupant).await.unwrap();

        let error = h.sup.start_job(start_msg(next)).await.unwrap_err();
        assert!(
            matches!(error.error_kind, connector::JobErrorKind::MaxConcurrentJobs),
            "{error:?}",
        );

        h.sup.remove_job(occupant).await.unwrap();
        h.sup.start_job(start_msg(next)).await.unwrap();
    }

    /// The stop hook is the start hook's counterpart: it runs once per job that
    /// ran the start hook, and never for a job that failed before it. It is
    /// part of releasing the job's resources, which the retention window defers
    /// until the removal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stop_hook_runs_once_and_only_after_the_start_hook() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness_with_hooks(virtual_size, virtual_size);
        let job_id = Uuid::new_v4();

        start_and_boot(&h, start_msg(job_id)).await;
        assert_eq!(hook_runs(&h.tmp, "start"), 1);
        assert_eq!(hook_runs(&h.tmp, "stop"), 0);

        h.sup.terminate_job(job_id).await.unwrap();
        assert_eq!(hook_runs(&h.tmp, "stop"), 0);

        h.sup.remove_job(job_id).await.unwrap();
        assert_eq!(hook_runs(&h.tmp, "start"), 1);
        assert_eq!(hook_runs(&h.tmp, "stop"), 1);

        // A job failing before the start hook has nothing for the stop hook to
        // clean up after.
        let failed = harness_with_hooks(8 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);
        let failed_job = Uuid::new_v4();
        failed.sup.start_job(start_msg(failed_job)).await.unwrap();
        let mut failed_facts = job_facts(&failed.sup).await;
        wait_for(&mut failed_facts, terminated).await;

        failed.sup.remove_job(failed_job).await.unwrap();
        assert_eq!(hook_runs(&failed.tmp, "start"), 0);
        assert_eq!(hook_runs(&failed.tmp, "stop"), 0);
    }

    /// Cancellation is structural, not polled: a stop that arrives while the
    /// job is inside an image fetch does not wait for that fetch to finish, and
    /// the fetch finishing afterwards does not run a second teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_terminated_mid_fetch_is_cancelled_where_it_stands() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let tmp = std::env::temp_dir().join(format!("tml-qemu-gate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let blob_file = tmp.join("root.qcow2");
        std::fs::write(&blob_file, b"not-a-real-qcow2").unwrap();

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let store = Arc::new(GatedStore {
            inner: StubStore {
                blob_file,
                manifest: single_layer_manifest(virtual_size),
            },
            entered: entered.clone(),
            release: release.clone(),
        });

        let h = harness_with_store(virtual_size, store);
        let job_id = Uuid::new_v4();

        h.sup.start_job(start_msg(job_id)).await.unwrap();
        let mut facts = job_facts(&h.sup).await;
        entered.notified().await;

        h.sup.terminate_job(job_id).await.unwrap();
        assert!(terminated(&facts.borrow_and_update()));
        assert!(h.launcher.spawned.lock().unwrap().is_empty());

        // Releasing the fetch afterwards must not resurrect the job.
        release.notify_waiters();
        h.sup.remove_job(job_id).await.unwrap();
        assert!(idle(&h.sup).await);

        let labels = h.connector.labels();
        assert_eq!(
            labels.iter().filter(|l| *l == "terminated").count(),
            1,
            "{labels:?}",
        );
        assert!(
            h.connector.errors().is_empty(),
            "{:?}",
            h.connector.errors()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A refused `StartJob` has no acknowledgement to fail: the command loop
    /// owes the coordinator a reported job error instead, or the refusal is
    /// never heard. The commands are answered in the order they arrive, so the
    /// acknowledged terminate behind them proves the refusal already happened.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_start_is_reported_as_a_job_error() {
        let virtual_size = 4u64 * 1024 * 1024 * 1024;
        let h = harness(virtual_size, virtual_size);

        let (commands, command_rx) = mpsc::channel(COORD_MAILBOX_CAPACITY);
        let sup = h.sup.clone();
        tokio::spawn(async move { sup.run(command_rx).await });

        let occupant = Uuid::new_v4();
        let refused = Uuid::new_v4();
        commands
            .send(connector::CoordCommand::StartJob(start_msg(occupant)))
            .await
            .unwrap();
        commands
            .send(connector::CoordCommand::StartJob(start_msg(refused)))
            .await
            .unwrap();

        let (ack, acked) = oneshot::channel();
        commands
            .send(connector::CoordCommand::TerminateJob {
                job_id: occupant,
                ack,
            })
            .await
            .unwrap();
        acked.await.unwrap().unwrap();

        let errors = h.connector.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(
                errors[0].error_kind,
                connector::JobErrorKind::AlreadyRunning
            ),
            "{:?}",
            errors[0],
        );
    }
}
