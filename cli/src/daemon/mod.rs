use std::collections::HashMap;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use log::{error, info, warn};
use zbus::interface;

use treadmill_rs::api::supervisor_daemon::{JobApi, SupervisorClient};
use treadmill_rs::api::switchboard::client::{ClientError, SwitchboardClient};
use treadmill_rs::api::switchboard::jobs::{JobEnvironment, JobGatewayEndpoint, JobParameter};
use uuid::Uuid;

mod service_proxy;

use service_proxy::{ServiceDeclaration, ServiceProxy, service_name_valid};

const FW_CFG_SUPERVISOR_URL: &str =
    "/sys/firmware/qemu_fw_cfg/by_name/opt/dev.treadmill.supervisor-url/raw";

const SWITCHBOARD_RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum DbusBus {
    Session,
    #[default]
    System,
    None,
}

#[derive(Debug, Clone, Args)]
pub struct DaemonArgs {
    #[arg(long)]
    supervisor_url: Option<String>,

    #[arg(long)]
    parameters_dir: Option<PathBuf>,

    #[arg(long)]
    job_info_dir: Option<PathBuf>,

    /// Directory of `*.json` service declarations to announce.
    #[arg(long)]
    services_dir: Option<PathBuf>,

    /// Where to write the generated reverse proxy vhost definitions. Without
    /// it, no proxy configuration is generated at all.
    #[arg(long)]
    caddy_config: Option<PathBuf>,

    /// Shell command run after the proxy configuration changed.
    #[arg(long)]
    caddy_reload_command: Option<String>,
}

/// The services declared under `--services-dir`, one [`ServiceDeclaration`] per
/// `*.json` file, ordered by name. Ignores and skips incorrect definitions with
/// warning.
async fn scan_services(services_dir: &Path) -> Result<Vec<ServiceDeclaration>> {
    let mut read_dir = match tokio::fs::read_dir(services_dir).await {
        Ok(read_dir) => read_dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("Service directory {services_dir:?} does not exist, announcing no services.");
            return Ok(Vec::new());
        }
        Err(e) => return Err(e).context("Reading the service directory"),
    };

    let mut paths = Vec::new();
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .context("Reading a service directory entry")?
    {
        let path = entry.path();
        if path.extension() == Some(std::ffi::OsStr::new("json")) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut services: Vec<ServiceDeclaration> = Vec::new();
    for path in paths {
        let contents = match tokio::fs::read(&path).await {
            Ok(contents) => contents,
            Err(e) => {
                warn!("Skipping unreadable service file {path:?}: {e}");
                continue;
            }
        };

        let declaration: ServiceDeclaration = match serde_json::from_slice(&contents) {
            Ok(declaration) => declaration,
            Err(e) => {
                warn!("Skipping malformed service file {path:?}: {e}");
                continue;
            }
        };

        if !service_name_valid(&declaration.service.name) {
            warn!(
                "Skipping service file {path:?}: {:?} is not a usable service name.",
                declaration.service.name
            );
            continue;
        }

        if services
            .iter()
            .any(|other| other.service.name == declaration.service.name)
        {
            warn!(
                "Skipping service file {path:?}: {:?} was already declared.",
                declaration.service.name
            );
            continue;
        }

        services.push(declaration);
    }

    services.sort_by(|a, b| a.service.name.cmp(&b.service.name));
    Ok(services)
}

/// Scan the service directory, put the local reverse proxy in front of what it
/// holds, and announce it, replacing whatever was announced before. A daemon with
/// no service directory configured announces nothing at all, rather than an empty
/// set.
///
/// The proxy is configured before the announcement, so a service is never
/// mintable at a gateway before the job can serve it. A proxy that could not be
/// configured is reported, but does not hold back the announcement: what the
/// switchboard knows about a job should not depend on the job's own proxy.
async fn announce_services(
    services_dir: Option<&Path>,
    proxy: Option<&ServiceProxy>,
    switchboard: &Switchboard,
) -> Result<()> {
    let Some(services_dir) = services_dir else {
        return Ok(());
    };

    let declarations = scan_services(services_dir)
        .await
        .context("Scanning the service directory")?;

    let proxy_res = match proxy {
        Some(proxy) => proxy
            .apply(&declarations)
            .await
            .context("Configuring the local service proxy"),
        None => Ok(()),
    };
    if let Err(ref e) = proxy_res {
        error!("Failed to configure the local service proxy: {e:?}");
    }

    info!(
        "Announcing {} service(s) from {services_dir:?}",
        declarations.len()
    );
    let services: Vec<_> = declarations
        .into_iter()
        .map(|declaration| declaration.service)
        .collect();
    switchboard
        .client
        .put_job_services(switchboard.job_id, &services)
        .await
        .context("Announcing the job's services to the switchboard")?;

    proxy_res
}

async fn update_job_info_files(
    args: &DaemonArgs,
    job_id: Uuid,
    environment: Option<&JobEnvironment>,
) -> Result<()> {
    let job_info_dir = match args.job_info_dir {
        Some(ref path) => path,
        None => return Ok(()),
    };

    tokio::fs::create_dir_all(job_info_dir)
        .await
        .context("Creating job_info_dir directory (recursively)")?;

    let job_id_path = job_info_dir.join("job-id");
    info!("Writing job id to file {job_id_path:?}");
    tokio::fs::write(job_id_path, job_id.to_string().as_bytes())
        .await
        .context("Writing job id to file")?;

    let Some(environment) = environment else {
        return Ok(());
    };

    let host_id_path = job_info_dir.join("host-id");
    info!("Writing host id to file {host_id_path:?}");
    tokio::fs::write(host_id_path, environment.host_id.to_string().as_bytes())
        .await
        .context("Writing host id to file")?;

    // Context for offering HTTP/WS services through public gateways.
    if let Some(gateway) = &environment.gateway {
        let issuer_path = job_info_dir.join("gateway-issuer");
        info!("Writing gateway issuer to file {issuer_path:?}");
        tokio::fs::write(issuer_path, gateway.issuer.as_bytes())
            .await
            .context("Writing gateway issuer to file")?;

        let key_path = job_info_dir.join("gateway-key.pem");
        info!("Writing gateway signing key to file {key_path:?}");
        tokio::fs::write(key_path, gateway.signing_public_key.as_bytes())
            .await
            .context("Writing gateway signing key to file")?;

        let key_id_path = job_info_dir.join("gateway-key-id");
        info!("Writing gateway key id to file {key_id_path:?}");
        tokio::fs::write(key_id_path, gateway.key_id.as_bytes())
            .await
            .context("Writing gateway key id to file")?;

        let endpoints_path = job_info_dir.join("gateway-endpoints");
        info!("Writing gateway endpoints to file {endpoints_path:?}");
        let endpoints: String = gateway
            .endpoints
            .iter()
            .map(|JobGatewayEndpoint { base_domain, port }| format!("{base_domain}:{port}\n"))
            .collect();
        tokio::fs::write(endpoints_path, endpoints.as_bytes())
            .await
            .context("Writing gateway endpoints to file")?;
    }

    // The admin's description of the machine this job runs on, as a document
    // rather than one file per field: it is nested, and a job reads it with
    // `jq` or a JSON parser.
    if let Some(host_spec) = &environment.host_spec {
        let host_spec_path = job_info_dir.join("host-spec.json");
        info!("Writing host spec to file {host_spec_path:?}");
        let document = serde_json::to_vec_pretty(host_spec).context("Serializing the host spec")?;
        tokio::fs::write(host_spec_path, document)
            .await
            .context("Writing host spec to file")?;
    }

    Ok(())
}

struct Switchboard {
    client: SwitchboardClient,
    base_url: String,
    token: String,
    job_id: Uuid,
}

impl Switchboard {
    fn new(job_id: Uuid, api: JobApi) -> Self {
        let token = api.token.into_inner();
        Switchboard {
            client: SwitchboardClient::new(api.base_url.clone(), Some(token.clone())),
            base_url: api.base_url,
            token,
            job_id,
        }
    }

    async fn environment(&self) -> Result<JobEnvironment> {
        loop {
            match self.client.get_job_environment(self.job_id).await {
                Ok(environment) => return Ok(environment),
                Err(ClientError::Transport(e)) => {
                    warn!(
                        "Cannot reach the switchboard, retrying in {SWITCHBOARD_RETRY_INTERVAL:?}: {e}"
                    );
                    tokio::time::sleep(SWITCHBOARD_RETRY_INTERVAL).await;
                }
                Err(e) => return Err(e).context("Fetching the job environment"),
            }
        }
    }
}

struct DbusDaemon {
    switchboard: Option<Arc<Switchboard>>,
    services_dir: Option<PathBuf>,
    proxy: Option<Arc<ServiceProxy>>,
}

impl DbusDaemon {
    fn switchboard(&self) -> zbus::fdo::Result<&Switchboard> {
        self.switchboard.as_deref().ok_or_else(|| {
            zbus::fdo::Error::Failed("This job has no access to a switchboard.".to_string())
        })
    }
}

#[interface(
    name = "dev.treadmill.Daemon1",
    proxy(
        gen_blocking = false,
        default_path = "/dev/treadmill/Daemon",
        default_service = "dev.treadmill.Daemon",
    )
)]
impl DbusDaemon {
    async fn credentials(&self) -> zbus::fdo::Result<(String, String, String)> {
        let switchboard = self.switchboard()?;
        Ok((
            switchboard.base_url.clone(),
            switchboard.token.clone(),
            switchboard.job_id.to_string(),
        ))
    }

    async fn reload_services(&self) -> zbus::fdo::Result<()> {
        info!("Received D-bus request to reload services, rescanning and announcing.");
        announce_services(
            self.services_dir.as_deref(),
            self.proxy.as_deref(),
            self.switchboard()?,
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

async fn update_parameters_dir(
    args: &DaemonArgs,
    parameters: &HashMap<String, JobParameter>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let parameters_dir_path = match args.parameters_dir {
        Some(ref path) => path,
        None => return Ok(()),
    };

    info!("Updating parameters dir: {parameters_dir_path:?}");

    // First, make sure that the directory exists:
    tokio::fs::create_dir_all(&parameters_dir_path)
        .await
        .context("Creating parameters dir (recursively)")?;

    // Write the parameters to a temporary file, and then atomically
    // rename this file to the target filename. This avoids reads of
    // partially written parameters:
    let tmpfile_path = parameters_dir_path.join(".tmp");
    for (name, value) in parameters {
        // Sanitize the path to ensure we don't have any
        // unrepresentable characters or path separators in there:
        let sanitized_path = parameters_dir_path.join(
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
                .take(128)
                .collect::<String>(),
        );

        // Dump the parameter value to a tempfile:
        let mut tmpfile = tokio::fs::File::create(&tmpfile_path)
            .await
            .context("Writing temporary parameter file")?;
        tmpfile
            .write_all(value.value.as_bytes())
            .await
            .context("Writing temporary parameter file")?;

        // To close the file immediately, we need to flush it and then
        // drop its handle:
        tmpfile
            .flush()
            .await
            .context("Flushing temporary parameter file")?;
        mem::drop(tmpfile);

        // Finally, rename the parameter to its sanitized path:
        tokio::fs::rename(&tmpfile_path, &sanitized_path)
            .await
            .with_context(|| {
                format!(
                    "Renaming temporary parameter file {tmpfile_path:?} to target file {sanitized_path:?}"
                )
            })?;
    }

    Ok(())
}

async fn supervisor_url(args: &DaemonArgs) -> Result<String> {
    match &args.supervisor_url {
        Some(url) => Ok(url.clone()),
        None => tokio::fs::read_to_string(FW_CFG_SUPERVISOR_URL)
            .await
            .with_context(|| {
                format!("No --supervisor-url given, and reading {FW_CFG_SUPERVISOR_URL} failed")
            }),
    }
}

async fn daemon_main(args: DaemonArgs, dbus_bus: DbusBus) -> Result<()> {
    let supervisor = SupervisorClient::new(supervisor_url(&args).await?);

    let job_info = supervisor
        .job_info()
        .await
        .context("Retrieving the job info from the supervisor")?;
    info!("Retrieved job info from supervisor: {job_info:?}");
    let job_id = job_info.job_id;
    let switchboard = job_info
        .api
        .map(|api| Arc::new(Switchboard::new(job_id, api)));

    let environment = match &switchboard {
        Some(switchboard) => Some(switchboard.environment().await?),
        None => {
            warn!("The supervisor handed out no switchboard access, running without it.");
            None
        }
    };
    update_job_info_files(&args, job_id, environment.as_ref()).await?;
    if let Some(environment) = &environment {
        update_parameters_dir(&args, &environment.parameters)
            .await
            .context("Failed to create / update parameters directory")?;
    }

    let gateway = environment.as_ref().and_then(|e| e.gateway.as_ref());
    let proxy = match (&args.caddy_config, gateway) {
        (Some(config_path), Some(gateway)) => Some(Arc::new(
            ServiceProxy::new(
                config_path.clone(),
                args.caddy_reload_command.clone(),
                job_id,
                gateway,
            )
            .context("Preparing the local service proxy")?,
        )),
        (Some(_), None) => {
            warn!(
                "A service proxy config was requested, but this job has no gateway. \
		 Not generating one."
            );
            None
        }
        (None, _) => None,
    };

    // Register as a DBus service:
    let dbus_builder_opt = match dbus_bus {
        DbusBus::Session => Some(zbus::connection::Builder::session()?),
        DbusBus::System => Some(zbus::connection::Builder::system()?),
        DbusBus::None => None,
    };

    let _dbus_conn = if let Some(dbus_builder) = dbus_builder_opt {
        Some(
            dbus_builder
                .name("dev.treadmill.Daemon")?
                .serve_at(
                    "/dev/treadmill/Daemon",
                    DbusDaemon {
                        switchboard: switchboard.clone(),
                        services_dir: args.services_dir.clone(),
                        proxy: proxy.clone(),
                    },
                )?
                .build()
                .await?,
        )
    } else {
        None
    };

    // Report the daemon as ready:
    supervisor
        .report_ready()
        .await
        .context("Reporting ready to the supervisor")?;

    // Announce whatever services the job declares at boot.
    if let Some(switchboard) = &switchboard {
        announce_services(args.services_dir.as_deref(), proxy.as_deref(), switchboard).await?;
    }

    info!("Daemon started. Exit with CTRL+C");
    sd_notify::notify(&[sd_notify::NotifyState::Ready])
        .context("Notifying service manager that the daemon is ready")?;

    tokio::signal::ctrl_c()
        .await
        .context("Unable to listen for shutdown signal")?;

    info!("Shutdown complete.");

    Ok(())
}

pub async fn run(args: DaemonArgs, dbus_bus: DbusBus) -> Result<()> {
    use simplelog::{
        ColorChoice, Config as SimpleLogConfig, LevelFilter, TermLogger, TerminalMode,
    };

    TermLogger::init(
        LevelFilter::Debug,
        SimpleLogConfig::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();

    daemon_main(args, dbus_bus).await
}

pub struct Credentials {
    pub base_url: String,
    pub token: String,
    pub job_id: Uuid,
}

async fn connect(dbus_bus: DbusBus) -> Result<Option<zbus::Connection>> {
    Ok(match dbus_bus {
        DbusBus::System => Some(zbus::Connection::system().await?),
        DbusBus::Session => Some(zbus::Connection::session().await?),
        DbusBus::None => None,
    })
}

pub async fn credentials(dbus_bus: DbusBus) -> Result<Option<Credentials>> {
    let Ok(Some(conn)) = connect(dbus_bus).await else {
        return Ok(None);
    };
    let owned = zbus::fdo::DBusProxy::new(&conn)
        .await?
        .name_has_owner("dev.treadmill.Daemon".try_into()?)
        .await?;
    if !owned {
        return Ok(None);
    }

    let (base_url, token, job_id) = DbusDaemonProxy::new(&conn)
        .await?
        .credentials()
        .await
        .context("Asking the tml daemon for its credentials")?;
    Ok(Some(Credentials {
        base_url,
        token,
        job_id: job_id
            .parse()
            .context("The tml daemon sent a malformed job id")?,
    }))
}

pub async fn reload_services(dbus_bus: DbusBus) -> Result<()> {
    let conn = connect(dbus_bus)
        .await?
        .context("Reloading services needs a D-Bus to reach the tml daemon on")?;
    DbusDaemonProxy::new(&conn)
        .await?
        .reload_services()
        .await
        .context("Requesting a service reload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use service_proxy::MAX_SERVICE_NAME_LEN;
    use treadmill_rs::api::switchboard::jobs::JobServiceAnnouncement;

    /// A scratch service directory, removed when the test ends.
    struct ServicesDir(PathBuf);

    impl Drop for ServicesDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl ServicesDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("tml-daemon-services-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            ServicesDir(path)
        }

        fn write(&self, file_name: &str, contents: &str) -> &Self {
            std::fs::write(self.0.join(file_name), contents).unwrap();
            self
        }
    }

    #[tokio::test]
    async fn a_declared_service_is_scanned() {
        let dir = ServicesDir::new();
        dir.write(
            "webide.json",
            r#"{"name": "webide", "label": "Web IDE", "protocol": "webapp"}"#,
        );

        assert_eq!(
            scan_services(&dir.0).await.unwrap(),
            vec![ServiceDeclaration {
                service: JobServiceAnnouncement {
                    name: "webide".to_string(),
                    label: Some("Web IDE".to_string()),
                    protocol: "webapp".to_string(),
                },
                upstream: None,
            }]
        );
    }

    /// A label is what a client displays, and a service need not have one.
    #[tokio::test]
    async fn a_label_is_optional() {
        let dir = ServicesDir::new();
        dir.write("shell.json", r#"{"name": "shell", "protocol": "sshws"}"#);

        let scanned = scan_services(&dir.0).await.unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].service.label, None);
    }

    /// The declarations are the job's own, so a bad one costs the job that one
    /// service and nothing else.
    #[tokio::test]
    async fn an_unusable_declaration_is_skipped_not_fatal() {
        let dir = ServicesDir::new();
        dir.write(
            "good.json",
            r#"{"name": "webide", "label": null, "protocol": "webapp"}"#,
        )
        .write("truncated.json", r#"{"name": "shell", "proto"#)
        .write("empty.json", "")
        .write(
            "uppercase.json",
            r#"{"name": "Webide", "label": null, "protocol": "webapp"}"#,
        )
        .write(
            "hyphenated.json",
            r#"{"name": "web-ide", "label": null, "protocol": "webapp"}"#,
        )
        .write(
            "toolong.json",
            &format!(
                r#"{{"name": "{}", "label": null, "protocol": "webapp"}}"#,
                "a".repeat(MAX_SERVICE_NAME_LEN + 1)
            ),
        )
        // Not a declaration at all: only `*.json` is read.
        .write("README.txt", "these are the services");

        let scanned = scan_services(&dir.0).await.unwrap();
        assert_eq!(scanned.len(), 1, "{scanned:?}");
        assert_eq!(scanned[0].service.name, "webide");
    }

    /// Two files may claim one name; the announcement may not, or the whole set
    /// is refused. Files are read in path order, so the same one always wins.
    #[tokio::test]
    async fn a_repeated_name_is_declared_once() {
        let dir = ServicesDir::new();
        dir.write(
            "a-first.json",
            r#"{"name": "webide", "label": "first", "protocol": "webapp"}"#,
        )
        .write(
            "b-second.json",
            r#"{"name": "webide", "label": "second", "protocol": "webapp"}"#,
        );

        let scanned = scan_services(&dir.0).await.unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].service.label.as_deref(), Some("first"));
    }

    /// The set is ordered by name, not by the order the directory happens to be
    /// read in.
    #[tokio::test]
    async fn the_scanned_set_is_ordered_by_name() {
        let dir = ServicesDir::new();
        dir.write(
            "1.json",
            r#"{"name": "shell", "label": null, "protocol": "sshws"}"#,
        )
        .write(
            "2.json",
            r#"{"name": "app", "label": null, "protocol": "webapp"}"#,
        )
        .write(
            "3.json",
            r#"{"name": "webide", "label": null, "protocol": "webapp"}"#,
        );

        let names: Vec<String> = scan_services(&dir.0)
            .await
            .unwrap()
            .into_iter()
            .map(|declaration| declaration.service.name)
            .collect();
        assert_eq!(names, ["app", "shell", "webide"]);
    }

    /// An image that declares nothing need not create the directory.
    #[tokio::test]
    async fn a_missing_directory_declares_nothing() {
        let dir = ServicesDir::new();
        let missing = dir.0.join("nonexistent");

        assert_eq!(scan_services(&missing).await.unwrap(), Vec::new());
    }
}
