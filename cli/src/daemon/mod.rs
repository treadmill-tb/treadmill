use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DbusBus {
    Session,
    System,
}

#[derive(Debug, Clone, Args)]
pub struct DaemonArgs {
    #[arg(long)]
    supervisor_url: Option<String>,

    #[arg(long, default_value = "/run/tml")]
    job_info_dir: PathBuf,

    /// Directory of `*.json` service declarations to announce.
    #[arg(long, default_value = "/etc/tml/services.d")]
    services_dir: PathBuf,

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

#[derive(Clone)]
pub struct Credentials {
    pub base_url: String,
    pub token: String,
    pub job_id: Uuid,
}

impl Credentials {
    pub fn client(&self) -> SwitchboardClient {
        SwitchboardClient::new(self.base_url.clone(), Some(self.token.clone()))
    }
}

async fn fetch_environment(credentials: &Credentials) -> Result<JobEnvironment> {
    let client = credentials.client();
    loop {
        match client.get_job_environment(credentials.job_id).await {
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

async fn write_file(dir: &Path, name: &str, contents: impl AsRef<[u8]>) -> Result<()> {
    let path = dir.join(name);
    let tmp_path = dir.join(format!(".{name}.tmp"));
    info!("Writing {path:?}");
    tokio::fs::write(&tmp_path, contents)
        .await
        .with_context(|| format!("Writing {tmp_path:?}"))?;
    tokio::fs::rename(&tmp_path, &path)
        .await
        .with_context(|| format!("Renaming {tmp_path:?} to {path:?}"))
}

async fn write_parameters(dir: &Path, parameters: &HashMap<String, JobParameter>) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .context("Creating the parameters directory")?;
    for (name, parameter) in parameters {
        let file_name: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
            .take(128)
            .collect();
        write_file(dir, &file_name, &parameter.value).await?;
    }
    Ok(())
}

#[derive(Clone)]
struct DbusDaemon {
    credentials: Credentials,
    services_dir: PathBuf,
    proxy: Option<ServiceProxy>,
}

impl DbusDaemon {
    async fn start(args: &DaemonArgs, job_id: Uuid, api: JobApi) -> Result<Self> {
        let credentials = Credentials {
            base_url: api.base_url,
            token: api.token.into_inner(),
            job_id,
        };
        let environment = fetch_environment(&credentials).await?;

        write_file(
            &args.job_info_dir,
            "host-id",
            environment.host_id.to_string(),
        )
        .await?;
        if let Some(gateway) = &environment.gateway {
            let endpoints: String = gateway
                .endpoints
                .iter()
                .map(|JobGatewayEndpoint { base_domain, port }| format!("{base_domain}:{port}\n"))
                .collect();
            write_file(&args.job_info_dir, "gateway-issuer", &gateway.issuer).await?;
            write_file(
                &args.job_info_dir,
                "gateway-key.pem",
                &gateway.signing_public_key,
            )
            .await?;
            write_file(&args.job_info_dir, "gateway-key-id", &gateway.key_id).await?;
            write_file(&args.job_info_dir, "gateway-endpoints", endpoints).await?;
        }
        if let Some(host_spec) = &environment.host_spec {
            let document =
                serde_json::to_vec_pretty(host_spec).context("Serializing the host spec")?;
            write_file(&args.job_info_dir, "host-spec.json", document).await?;
        }
        write_parameters(
            &args.job_info_dir.join("parameters"),
            &environment.parameters,
        )
        .await?;

        let proxy = match (&args.caddy_config, &environment.gateway) {
            (Some(config_path), Some(gateway)) => Some(
                ServiceProxy::new(
                    config_path.clone(),
                    args.caddy_reload_command.clone(),
                    job_id,
                    gateway,
                )
                .context("Preparing the local service proxy")?,
            ),
            _ => None,
        };

        Ok(DbusDaemon {
            credentials,
            services_dir: args.services_dir.clone(),
            proxy,
        })
    }

    /// The proxy is configured before the announcement, so a service is never
    /// mintable at a gateway before the job can serve it. A proxy that could not be
    /// configured is reported, but does not hold back the announcement: what the
    /// switchboard knows about a job should not depend on the job's own proxy.
    async fn announce_services(&self) -> Result<()> {
        let declarations = scan_services(&self.services_dir)
            .await
            .context("Scanning the service directory")?;

        let proxy_res = match &self.proxy {
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
            "Announcing {} service(s) from {:?}",
            declarations.len(),
            self.services_dir
        );
        let services: Vec<_> = declarations
            .into_iter()
            .map(|declaration| declaration.service)
            .collect();
        self.credentials
            .client()
            .put_job_services(self.credentials.job_id, &services)
            .await
            .context("Announcing the job's services to the switchboard")?;

        proxy_res
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
    async fn credentials(&self) -> (String, String, String) {
        (
            self.credentials.base_url.clone(),
            self.credentials.token.clone(),
            self.credentials.job_id.to_string(),
        )
    }

    async fn reload_services(&self) -> zbus::fdo::Result<()> {
        info!("Received D-bus request to reload services, rescanning and announcing.");
        self.announce_services()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
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

async fn connect(dbus_bus: DbusBus) -> zbus::Result<zbus::Connection> {
    match dbus_bus {
        DbusBus::System => zbus::Connection::system().await,
        DbusBus::Session => zbus::Connection::session().await,
    }
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

    let supervisor = SupervisorClient::new(supervisor_url(&args).await?);
    let job_info = supervisor
        .job_info()
        .await
        .context("Retrieving the job info from the supervisor")?;
    info!("Retrieved job info from supervisor: {job_info:?}");

    tokio::fs::create_dir_all(&args.job_info_dir)
        .await
        .context("Creating the job info directory")?;
    write_file(&args.job_info_dir, "job-id", job_info.job_id.to_string()).await?;

    let daemon = match job_info.api {
        Some(api) => Some(DbusDaemon::start(&args, job_info.job_id, api).await?),
        None => {
            warn!("The supervisor handed out no switchboard access, running without it.");
            None
        }
    };

    let _dbus_conn = match &daemon {
        Some(daemon) => {
            let conn = connect(dbus_bus).await?;
            conn.object_server()
                .at("/dev/treadmill/Daemon", daemon.clone())
                .await?;
            conn.request_name("dev.treadmill.Daemon").await?;
            Some(conn)
        }
        None => None,
    };

    supervisor
        .report_ready()
        .await
        .context("Reporting ready to the supervisor")?;

    if let Some(daemon) = &daemon {
        daemon.announce_services().await?;
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

pub async fn credentials(dbus_bus: DbusBus) -> Result<Option<Credentials>> {
    let Ok(conn) = connect(dbus_bus).await else {
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
    DbusDaemonProxy::new(&connect(dbus_bus).await?)
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
