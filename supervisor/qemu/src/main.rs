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
