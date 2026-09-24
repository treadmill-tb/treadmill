use std::collections::HashMap;
use std::ffi::OsString;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use log::{debug, error, info, warn};
use zbus::interface;

use treadmill_rs::api::supervisor_puppet::{
    CommandOutputStream, JobApi, PuppetEvent, SupervisorEvent,
};
use treadmill_rs::api::switchboard::client::{ClientError, SwitchboardClient};
use treadmill_rs::api::switchboard::jobs::{JobEnvironment, JobGatewayEndpoint, JobParameter};
use uuid::Uuid;

mod control_socket_client;
mod service_proxy;

use service_proxy::{ServiceDeclaration, ServiceProxy, service_name_valid};

// Cache at most 1024 supervisor-sent events:
const SUPERVISOR_EVENT_CHANNEL_CAP: usize = 1024;

const SWITCHBOARD_RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum ControlSocketTransport {
    Tcp,
    AutoDiscover,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum DbusBus {
    Session,
    #[default]
    System,
    None,
}

#[derive(Debug, Clone, Args)]
pub struct DaemonArgs {
    #[arg(long, short = 't')]
    transport: ControlSocketTransport,

    #[arg(long, required_if_eq("transport", "tcp"))]
    tcp_control_socket_addr: Option<std::net::SocketAddr>,

    #[arg(long)]
    network_config_script: Option<PathBuf>,

    #[arg(long, default_value = "true")]
    exit_on_network_config_error: bool,

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

async fn configure_network(
    args: &DaemonArgs,
    client: &control_socket_client::ControlSocketClient,
) -> Result<()> {
    // Request the network configuration, dump it into environment variables and
    // pass it onto the network configuration script, if one is provided.
    //
    // Some environments require the network (or at least one address family) to
    // be bootstrapped in order to establish a control socket connection at
    // all. Thus, only the hostname parameter is manadatory for any network
    // configuration object provided by the supervisor.
    if let Some(script) = &args.network_config_script {
        info!("Requesting network configuration from supervisor.");

        let network_config = client
            .get_network_config()
            .await
            .context("Requesting network config from supervisor")?;

        let mut cmd = tokio::process::Command::new(script);
        cmd.stdin(Stdio::null());
        cmd.env("HOSTNAME", &network_config.hostname);

        if let Some(ref iface) = network_config.interface {
            cmd.env("INTERFACE", iface);
        }

        if let Some(ref v4_config) = network_config.ipv4 {
            cmd.env("IPV4_ADDRESS", format!("{}", v4_config.address));
            cmd.env("IPV4_PREFIX_LENGTH", format!("{}", v4_config.prefix_length));
            if let Some(ref v4_gw) = v4_config.gateway {
                cmd.env("IPV4_GATEWAY", format!("{v4_gw}"));
            }
            let nameserver_str: String = v4_config
                .nameservers
                .iter()
                .map(|addr| format!("{addr}"))
                // This is much cleaner with the nightly-only .intersperse
                .fold(String::new(), |acc, nameserver| {
                    let sep = if !acc.is_empty() { "|" } else { "" };
                    acc + sep + &nameserver
                });
            cmd.env("IPV4_NAMESERVERS", nameserver_str);
        }

        if let Some(ref v6_config) = network_config.ipv6 {
            cmd.env("IPV6_ADDRESS", format!("{}", v6_config.address));
            cmd.env("IPV6_PREFIX_LENGTH", format!("{}", v6_config.prefix_length));
            if let Some(ref v6_gw) = v6_config.gateway {
                cmd.env("IPV6_GATEWAY", format!("{v6_gw}"));
            }
            let nameserver_str: String = v6_config
                .nameservers
                .iter()
                .map(|addr| format!("{addr}"))
                // This is much cleaner with the nightly-only .intersperse
                .fold(String::new(), |acc, nameserver| {
                    let sep = if !acc.is_empty() { "|" } else { "" };
                    acc + sep + &nameserver
                });
            cmd.env("IPV6_NAMESERVERS", nameserver_str);
        }

        info!("Updating network configuration using the provided configuration script: {script:?}");
        match cmd.spawn() {
            Ok(mut child) => match child.wait().await {
                Ok(status) => {
                    if let Some(code) = status.code() {
                        if code == 0 {
                            info!("Successfully configured networking.");
                        } else {
                            bail!(
                                "Network configuration script reported non-zero exit status: {}",
                                code
                            );
                        }
                    } else {
                        bail!("Network configuration script terminated by a signal.");
                    }
                }
                Err(e) => {
                    bail!("Error running network configuration script: {:?}", e);
                }
            },

            Err(e) => {
                bail!("Error spawning network configuration script: {:?}", e);
            }
        }
    }

    Ok(())
}

enum CommandExecutorMsg {
    Kill,
}

async fn run_command(
    client: Weak<control_socket_client::ControlSocketClient>,
    event_id: u64,
    cmdline: Vec<u8>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    mut command_executor_rx: tokio::sync::mpsc::Receiver<CommandExecutorMsg>,
) -> Result<(Option<i32>, bool)> {
    // All errors we return will be reported to the coordinator by the caller.
    use std::os::unix::ffi::OsStringExt;
    use tokio::io::AsyncReadExt;

    // The supervisor must provide us the cmdline and environment variables in
    // an encoding that we're able to convert into an OsStr.
    //
    // Right now, we're targeting only UNIX systems (guarded by the
    // std::os::unix::ffi::OsStringExt import), and as such this is
    // infallible. We'll need to figure out a different story for when we ever
    // support Windows:
    let cmdline_osstr = OsString::from_vec(cmdline);

    let mut cmd = tokio::process::Command::new(&cmdline_osstr);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    for (env_var_name, env_var_val) in environment.into_iter() {
        cmd.env(
            OsString::from_vec(env_var_name),
            OsString::from_vec(env_var_val),
        );
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Spawning child process {:?}", cmdline_osstr))?;

    // Acquire the stdout and stderr pipes, and spawn a new log-streamer
    // task that collects all log output and streams it to the coordinator:
    //
    // We use expect here, as this should always work:
    let stdout = child
        .stdout
        .take()
        .expect("Failed to acquire stdout from child process");
    let stderr = child
        .stderr
        .take()
        .expect("Failed to acquire stderr from child process");

    // BufReader capacity, used for both stdout and stderr, and for the
    // `Vec`s that the BufReader's contents are read into:
    const CONSOLE_READER_BUF_CAPACITY: usize = 16 * 1024;

    // Create BufReaders from the file descriptors for streaming:
    let mut stdout_reader =
        tokio::io::BufReader::with_capacity(CONSOLE_READER_BUF_CAPACITY, stdout);
    let mut stderr_reader =
        tokio::io::BufReader::with_capacity(CONSOLE_READER_BUF_CAPACITY, stderr);

    // This is pretty inefficient, the `BufReader` already reads into a
    // buffer. Ideally we'd like to have a method that copies an `AsyncRead`
    // into an `AsyncWrite`, but returns after the _first_ `Poll::Ready` on
    // the underlying reader.
    let mut stdout_buf = vec![0; CONSOLE_READER_BUF_CAPACITY];
    let mut stdout_closed = false;
    let mut stderr_buf = [0; CONSOLE_READER_BUF_CAPACITY];
    let mut stderr_closed = false;

    enum ReadConsoleRes {
        ZeroBytes,
        Data(CommandOutputStream, Vec<u8>),
        Error(std::io::Error),
        IntervalFired,
        CommandExecutorChan(CommandExecutorMsg),
    }

    // Create an interval to check whether the process is still alive at least
    // every 100ms (or faster, if we get other events)
    let mut proc_check_interval = tokio::time::interval(Duration::from_millis(100));

    // When we've been requested to kill the subprocess, this Option will be set
    // to an `Instant` in the future at which we'll SIGKILL the child if it
    // doesn't terminate on its own.
    let mut sigkill_at = None;

    loop {
        // TODO: force buf flush on timeout?
        #[rustfmt::skip]
        let res = tokio::select! {
            command_exector_msg_opt = command_executor_rx.recv() => {
                match command_exector_msg_opt {
                    Some(msg) => ReadConsoleRes::CommandExecutorChan(msg),
                    None => {
			// This should never happen, it must only be dropped
			// from the HashMap when this method has exited:
                        panic!("Command executor channel TX dropped!");
                    }
                }
            }

            read_res = stdout_reader.read(&mut stdout_buf), if !stdout_closed => {
                match read_res {
                    Ok(0) => {
                        // Mark as closed, so we don't loop reading zero bytes:
                        stdout_closed = true;
                        ReadConsoleRes::ZeroBytes
                    },
                    Ok(read_len) => {
                        ReadConsoleRes::Data(
                            CommandOutputStream::Stdout,
                            stdout_buf[..read_len].to_vec()
                        )
                    }
                    Err(e) => ReadConsoleRes::Error(e),
                }
            }

            read_res = stderr_reader.read(&mut stderr_buf), if !stderr_closed => {
                match read_res {
                    Ok(0) => {
                        // Mark as closed, so we don't loop reading zero bytes:
                        stderr_closed = true;
                        ReadConsoleRes::ZeroBytes
                    },
                    Ok(read_len) => {
                        ReadConsoleRes::Data(
                            CommandOutputStream::Stderr,
                            stderr_buf[..read_len].to_vec()
                        )
                    },
                    Err(e) => ReadConsoleRes::Error(e),
                }
            }

	    _ = proc_check_interval.tick() => {
		ReadConsoleRes::IntervalFired
	    }
        };

        match res {
            ReadConsoleRes::Data(stream, data) => {
                // Post this data to the supervisor. Sending an event is
                // asynchronous, so we don't block ourselves from reading
                // more data here. However, this could also get quite spammy
                // -- if this ends up being an issue, we should introduce
                // some form of rate limiting here.

                // Get a temporary "strong" reference to the control socket
                // client, such that we can send the event:
                if let Some(c) = client.upgrade() {
                    let res = c
                        .send_event(PuppetEvent::RunCommandOutput {
                            supervisor_event_id: event_id,
                            output: data,
                            stream,
                        })
                        .await;

                    if let Err(e) = res {
                        warn!(
                            "Failed to send command log output to \
			     supervisor, discarding: {e:?}",
                        );
                    }
                } else {
                    warn!(
                        "Discarding command log output, unable to upgrade \
			 control socket client weak reference (currently \
			 being shut down?)"
                    );
                }
            }

            ReadConsoleRes::Error(e) => {
                panic!("Unhandled error reading process output: {e:?}");
            }

            ReadConsoleRes::CommandExecutorChan(CommandExecutorMsg::Kill) => {
                // Asked to kill the subprocess. We don't yet have a way of
                // specifying exactly how the subprocess should be killed
                // (e.g. SIGTERM or SIGKILL), so we'll start with a graceful
                // terminate request and then proceed to kill with SIGKILL:
                if sigkill_at.is_none() {
                    // Send a SIGTERM first:
                    if let Some(pid) = child.id() {
                        info!("Sending SIGTERM to command #{event_id} (PID {pid:?})");
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid.try_into().unwrap()),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                    }

                    // Set the sigkill_at timeout to 30 sec from now:
                    sigkill_at = Some(Instant::now() + Duration::from_secs(30));
                }
            }

            ReadConsoleRes::ZeroBytes => {
                // Reading zero bytes can happen when file descriptors are
                // closed, and thus is an indication that the process might've
                // exited.
                //
                // When we read zero bytes on any file descriptor above, we
                // avoid reading from it again. Thus, we don't need to worry
                // about busy-looping and reading zero bytes over and over
                // again.
                //
                // As this is just an indication that the child process died, we
                // already handle this logic below. Don't need to do anything
                // special here.
            }

            ReadConsoleRes::IntervalFired => {
                // Used to regularly check for process state changes, such as
                // whether it has died, or to perform delayed tasks, such as
                // terminating it with a SIGKILL.
                //
                // Handled below, don't need to special-case here.
            }
        };

        // Whenever we break out of the async select!, either because we've read
        // zero bytes or there was a timeout, check whether the child has
        // already exited.
        let exit_status = match child.try_wait() {
            // The child has exited:
            Ok(Some(exit_status)) => Some(exit_status),
            // The child has not exited:
            Ok(None) => None,
            // Couldn't determine the exit status:
            Err(e) => {
                panic!("Error while determining whether child exited: {e:?}");
            }
        };

        // If it has, we'll just perform some cleanup:
        if let Some(es) = exit_status {
            break Ok((es.code(), sigkill_at.is_some()));
        }

        // If not, check whether it's time to SIGKILL it:
        if let Some(t) = sigkill_at
            && t < Instant::now()
        {
            // Send a SIGKILL to the child:
            info!("Sending SIGKILL to command #{event_id}");
            child.kill().await.context("Killing command with SIGKILL")?;

            // Report that the child has been killed with SIGKILL:
            break Ok((None, true));
        }
    }
}

async fn daemon_main(args: DaemonArgs, dbus_bus: DbusBus) -> Result<()> {
    let mut client = Arc::new(
        async {
            match args.transport {
                ControlSocketTransport::Tcp => Ok(control_socket_client::ControlSocketClient::Tcp(
                    control_socket_client::tcp::TcpControlSocketClient::new(
                        args.tcp_control_socket_addr.unwrap(),
                        SUPERVISOR_EVENT_CHANNEL_CAP,
                    )
                    .await?,
                )),

                ControlSocketTransport::AutoDiscover => {
                    // Give all known control socket clients a chance to auto-discover,
                    // in no particular order:
                    if let Some(client_res) =
                        control_socket_client::tcp::TcpControlSocketClient::autodiscover(
                            SUPERVISOR_EVENT_CHANNEL_CAP,
                        )
                        .await
                    {
                        return Ok(control_socket_client::ControlSocketClient::Tcp(client_res?));
                    }

                    // We did not autodiscover a control socket to connect to, give up:
                    Err(anyhow!("Auto-discovery of control socket endpoint failed."))
                }
            }
        }
        .await?,
    );

    let job_info = client
        .get_job_info()
        .await
        .context("Retrieving job_id from supervisor")?;
    info!("Retrieved job info message from supervisor: {job_info:?}");
    let job_id = job_info.job_id;
    let switchboard = job_info
        .api
        .map(|api| Arc::new(Switchboard::new(job_id, api)));

    // For certain requests and depending on some command line parameters, we'll
    // want to exit with an error if they fail. We provided these wrappers here
    // that selectively either log or forward errors:

    async fn configure_network_wrapper(
        args: &DaemonArgs,
        client: &control_socket_client::ControlSocketClient,
    ) -> Result<()> {
        let msg = "Failed to configure the network using the provided script";
        let res = configure_network(args, client).await;

        if args.exit_on_network_config_error {
            // Forward the raw Result with additional context:
            res.context(msg)
        } else if let Err(e) = res {
            // Simply log errors with the context part of the log message:
            warn!("{msg}: {e:?}");
            Ok(())
        } else {
            Ok(())
        }
    }

    // We perform a couple essential supervisor requests at the start, report
    // ourselves as ready, and then listen to supervisor events.

    configure_network_wrapper(&args, &client).await?;

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
    client
        .report_ready()
        .await
        .context("Reporting daemon ready status to supervisor")?;

    // Announce whatever services the job declares at boot.
    if let Some(switchboard) = &switchboard {
        announce_services(args.services_dir.as_deref(), proxy.as_deref(), switchboard).await?;
    }

    info!("Daemon started, waiting for supervisor events. Exit with CTRL+C");
    sd_notify::notify(&[sd_notify::NotifyState::Ready])
        .context("Notifying service manager that the daemon is ready")?;

    // Create a HashMap with channels to the executor of a command which is
    // shared between all executors and this main loop. The purpose of this
    // shared map is that executors can remove themselves from it once their
    // command finished executing:
    let executor_channels: Arc<
        tokio::sync::Mutex<HashMap<u64, tokio::sync::mpsc::Sender<CommandExecutorMsg>>>,
    > = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    loop {
        #[rustfmt::skip]
	let (event_id, event) = tokio::select! {
	    ctrlc_res = tokio::signal::ctrl_c() => {
		// We exit in case of error:
		ctrlc_res.context("Unable to listen for shutdown signal")?;

		// If we don't get an error, break from this loop:
		break;
	    }

	    ev_res = client.listen() => {
		// We exit in case listening for events fails:
		ev_res.context("Listening for supervisor events.")?
	    }
	};

        debug!("Received supervisor event: {:?}", event);

        match event {
            SupervisorEvent::ShutdownReq => {
                warn!("Supervisor requested shutdown, not implemented yet!");
            }

            SupervisorEvent::RebootReq => {
                warn!("Supervisor requested reboot, not implemented yet!");
            }

            SupervisorEvent::RunCommand {
                cmdline,
                environment,
            } => {
                info!(
                    "Supervisor requests running command (id #{}), spawning in background: \"{}\"",
                    event_id,
                    String::from_utf8_lossy(&cmdline)
                );

                // Create a channel for this command executor and insert the TX
                // end into the shared HashMap. We limit ourselves to 64
                // outstanding requests (but really should never reach this
                // number under normal circumstances).
                let command_executor_rx_opt = {
                    let (command_executor_tx, command_executor_rx) = tokio::sync::mpsc::channel(64);

                    // This can be more elegant with the Nightly-only `try_insert`:
                    let mut ec_lg = executor_channels.lock().await;
                    if let std::collections::hash_map::Entry::Vacant(e) = ec_lg.entry(event_id) {
                        e.insert(command_executor_tx);
                        Some(command_executor_rx)
                    } else {
                        None
                    }
                };

                if let Some(command_executor_rx) = command_executor_rx_opt {
                    let executor_channels_cloned = executor_channels.clone();
                    let client_weak = Arc::downgrade(&client);
                    tokio::task::spawn(async move {
                        let res = run_command(
                            client_weak.clone(),
                            event_id,
                            cmdline,
                            environment,
                            command_executor_rx,
                        )
                        .await;

                        // Command finished. Report error or retcode:
                        match res {
                            Err(e) => {
                                warn!(
                                    "Failed to run command #{event_id}: {e:?}, reporting back to supervisor."
                                );
                                let send_res = match client_weak
				    .upgrade()
				    .ok_or(anyhow!("Cannot upgrade weak client ref to report command error back to supervisor"))
				{
				    Ok(c) => c.send_event(
					PuppetEvent::RunCommandError {
					    supervisor_event_id: event_id,
					    error: format!("{e:?}"),
					}).await,
				    Err(e) => Err(e),
				};

                                if let Err(send_e) = send_res {
                                    warn!(
                                        "Failed reporting command #{event_id} error back to supervisor: {send_e:?}"
                                    );
                                }
                            }
                            Ok((exit_code, killed)) => {
                                info!(
                                    "Finished command #{event_id} with return code {exit_code:?}, reporting back to supervisor."
                                );

                                let send_res = match client_weak
				    .upgrade()
				    .ok_or(anyhow!("Cannot upgrade weak client ref to report command error back to supervisor"))
				{
				    Ok(c) => c.send_event(PuppetEvent::RunCommandExitCode {
				    supervisor_event_id: event_id,
				    exit_code,
				    killed,
				    }).await,
				    Err(e) => Err(e),
				};

                                if let Err(send_e) = send_res {
                                    warn!(
                                        "Failed reporting command #{event_id} exit status back to supervisor: {send_e:?}"
                                    );
                                }
                            }
                        }

                        // Either way, the command finished. Remove the tx
                        // channel from the executor channels map. We drop the
                        // lock immediately afterwards:
                        assert!(
                            executor_channels_cloned
                                .lock()
                                .await
                                .remove(&event_id)
                                .is_some()
                        );
                    });
                } else {
                    error!(
                        "Supervisor requested starting command with ID {event_id}, but \
			 such a command is already running! Discarding this \
			 request.",
                    );
                }
            }

            SupervisorEvent::KillCommand {
                supervisor_event_id,
            } => {
                info!("Supervisor requested to kill command with id #{supervisor_event_id}");

                if let Some(command_executor_tx) =
                    executor_channels.lock().await.get(&supervisor_event_id)
                {
                    if let Err(e) = command_executor_tx.try_send(CommandExecutorMsg::Kill) {
                        warn!(
                            "Failed to forward kill-request to command executor #{supervisor_event_id}: {e:?}"
                        );
                    }
                } else {
                    warn!(
                        "Command executor for id #{supervisor_event_id} not found. Perhaps it's already dead?"
                    );
                }
            }

            _ => {
                warn!("Received unhandled supervisor event (id #{event_id}): {event:?}");
            }
        }
    }

    info!(
        "Shutting down, waiting for all other active control socket client references to go out of scope..."
    );

    loop {
        match Arc::try_unwrap(client) {
            Err(returned_client) => {
                // Put the client back:
                client = returned_client;

                // Wait for a bit, try again:
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            Ok(returned_client) => {
                // We hold the last, owned reference to client, initiate
                // shutdown:
                returned_client
                    .shutdown()
                    .await
                    .context("Shutting down the control socket client")?;
                break;
            }
        }
    }

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
