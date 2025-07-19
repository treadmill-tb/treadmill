use std::collections::HashMap;
use std::ffi::OsString;
use std::mem;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use log::{debug, error, info, warn};
use zbus::interface;

use treadmill_rs::api::supervisor_puppet::{
    CommandOutputStream, JobInfo, PuppetEvent, SupervisorEvent,
};

mod control_socket_client;

// Cache at most 1024 supervisor-sent events:
const SUPERVISOR_EVENT_CHANNEL_CAP: usize = 1024;

#[derive(Debug, Clone, ValueEnum)]
#[clap(rename_all = "snake_case")]
enum PuppetControlSocketTransport {
    #[cfg(feature = "transport_tcp")]
    Tcp,
    AutoDiscover,
}

#[derive(Debug, Clone, ValueEnum)]
enum PuppetDaemonDbusBus {
    Session,
    System,
    None,
}

#[derive(Debug, Clone, ValueEnum)]
enum PuppetDbusBus {
    Session,
    System,
}

#[derive(Debug, Clone, Parser)]
struct ClientBusOptions {
    /// The D-Bus to connect to:
    #[arg(long, default_value = "system")]
    dbus_bus: PuppetDbusBus,
}

#[derive(Debug, Clone, Parser)]
struct PuppetDaemonArgs {
    #[arg(long, short = 't')]
    transport: PuppetControlSocketTransport,

    #[arg(long, required_if_eq("transport", "tcp"))]
    tcp_control_socket_addr: Option<std::net::SocketAddr>,

    #[arg(long)]
    authorized_keys_file: Option<PathBuf>,

    #[arg(long, default_value = "true")]
    exit_on_authorized_keys_update_error: bool,

    #[arg(long)]
    network_config_script: Option<PathBuf>,

    #[arg(long, default_value = "true")]
    exit_on_network_config_error: bool,

    #[arg(long)]
    parameters_dir: Option<PathBuf>,

    #[arg(long)]
    job_info_dir: Option<PathBuf>,

    #[arg(long, default_value = "system")]
    dbus_bus: PuppetDaemonDbusBus,
}

async fn update_job_info_files(args: &PuppetDaemonArgs, job_info: JobInfo) -> Result<()> {
    let job_info_dir = match args.job_info_dir {
        Some(ref path) => path,
        None => return Ok(()),
    };

    tokio::fs::create_dir_all(job_info_dir)
        .await
        .context("Creating job_info_dir directory (recursively)")?;

    let job_id_path = job_info_dir.join("job-id");
    info!("Writing job id to file {job_id_path:?}");
    tokio::fs::write(job_id_path, job_info.job_id.to_string().as_bytes())
        .await
        .context("Writing job id to file")?;

    let host_id_path = job_info_dir.join("host-id");
    info!("Writing host id to file {host_id_path:?}");
    tokio::fs::write(host_id_path, job_info.host_id.to_string().as_bytes())
        .await
        .context("Writing host id to file")?;

    Ok(())
}

#[derive(Debug, Clone, Parser)]
struct PuppetJobTerminateCommand;

#[derive(Debug, Clone, Subcommand)]
enum PuppetJobSubcommands {
    /// Request the current job to be terminated.
    Terminate(PuppetJobTerminateCommand),
}

#[derive(Debug, Clone, Parser)]
struct PuppetJobCommand {
    #[clap(flatten)]
    bus_options: ClientBusOptions,

    #[clap(subcommand)]
    job_command: PuppetJobSubcommands,
}

#[derive(Debug, Clone, Subcommand)]
enum PuppetCommands {
    /// Run a puppet daemon, connecting to a supervisor control socket
    /// and establishing a DBus socket.
    Daemon(PuppetDaemonArgs),

    /// Commands related to the job currently executed on this supervisor.
    Job(PuppetJobCommand),
}

#[derive(Debug, Clone, Parser)]
struct PuppetCli {
    #[clap(subcommand)]
    puppet_command: PuppetCommands,
}

struct DbusPuppet {
    control_socket_client: Arc<control_socket_client::ControlSocketClient>,
}

#[interface(
    name = "ci.treadmill.Puppet1",
    proxy(
        gen_blocking = false,
        default_path = "/ci/treadmill/Puppet",
        default_service = "ci.treadmill.Puppet",
    )
)]
impl DbusPuppet {
    async fn terminate_job(&self) -> zbus::fdo::Result<()> {
        info!("Received D-bus request to terminate job, forwarding to supervisor.");
        self.control_socket_client
            .terminate_job()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

async fn update_parameters_dir(
    args: &PuppetDaemonArgs,
    client: &control_socket_client::ControlSocketClient,
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

    // Fetch the set of parameters:
    let parameters = client
        .get_parameters()
        .await
        .context("Requesting parameters from supervisor")?;

    // Write the parameters to a temporary file, and then atomically
    // rename this file to the target filename. This avoids reads of
    // partially written parameters:
    let tmpfile_path = parameters_dir_path.join(".tmp");
    for (name, value) in parameters.into_iter() {
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

async fn update_authorized_keys(
    args: &PuppetDaemonArgs,
    client: &control_socket_client::ControlSocketClient,
) -> Result<()> {
    if let Some(ref authorized_keys_file) = args.authorized_keys_file {
        info!("Updating SSH authorized_keys file: {authorized_keys_file:?}");

        // Request the set of SSH authorized keys from the supervisor:
        let ssh_keys = client
            .get_ssh_keys()
            .await
            .context("Requesting SSH keys from supervisor")?;

        // Create the authorized keys file's parent directories (if they
        // don't exist) and dump the keys to the file:
        tokio::fs::create_dir_all(authorized_keys_file.parent().ok_or_else(|| {
            anyhow!(
                "Failed to determine parent directory of authorized_keys file: {:?}",
                authorized_keys_file
            )
        })?)
        .await
        .with_context(|| {
            format!("Creating parent directories of authorized_keys file {authorized_keys_file:?}")
        })?;

        let authorized_keys = format!(
            "# WARNING: this file is managed by tml-puppet and may be overwritten.\n\
	     # Please add your own SSH keys to an alternative authorized_keys file\n\
	     # (such as ~/.ssh/authorized_keys2)\n\
	     {}\n",
            ssh_keys.join("\n"),
        );
        tokio::fs::write(authorized_keys_file, authorized_keys.as_bytes())
            .await
            .with_context(|| {
                format!(
                    "Writing authorized_keys ({} bytes) to {:?}",
                    authorized_keys.len(),
                    authorized_keys_file
                )
            })?;

        info!(
            "Received {} SSH authorized_keys from supervisor, file updated successfully.",
            ssh_keys.len()
        );
    }

    Ok(())
}

async fn configure_network(
    args: &PuppetDaemonArgs,
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
        .with_context(|| format!("Spawning child process {:?}", &cmdline_osstr))?;

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
        if let Some(t) = sigkill_at {
            if t < Instant::now() {
                // Send a SIGKILL to the child:
                info!("Sending SIGKILL to command #{event_id}");
                child.kill().await.context("Killing command with SIGKILL")?;

                // Report that the child has been killed with SIGKILL:
                break Ok((None, true));
            }
        }
    }
}

async fn daemon_main(args: PuppetDaemonArgs) -> Result<()> {
    let mut client = Arc::new(
        async {
            match args.transport {
                #[cfg(feature = "transport_tcp")]
                PuppetControlSocketTransport::Tcp => {
                    Ok(control_socket_client::ControlSocketClient::Tcp(
                        control_socket_client::tcp::TcpControlSocketClient::new(
                            args.tcp_control_socket_addr.unwrap(),
                            SUPERVISOR_EVENT_CHANNEL_CAP,
                        )
                        .await?,
                    ))
                }

                PuppetControlSocketTransport::AutoDiscover => {
                    // Give all known control socket clients a chance to auto-discover,
                    // in no particular order:
                    #[cfg(feature = "transport_tcp")]
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
    update_job_info_files(&args, job_info).await?;

    // For certain requests and depending on some command line parameters, we'll
    // want to exit with an error if they fail. We provided these wrappers here
    // that selectively either log or forward errors:

    async fn update_authorized_keys_wrapper(
        args: &PuppetDaemonArgs,
        client: &control_socket_client::ControlSocketClient,
    ) -> Result<()> {
        let msg = "Failed to update the SSH authorized_keys database";
        let res = update_authorized_keys(args, client).await;

        if args.exit_on_authorized_keys_update_error {
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

    async fn configure_network_wrapper(
        args: &PuppetDaemonArgs,
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

    update_authorized_keys_wrapper(&args, &client).await?;
    configure_network_wrapper(&args, &client).await?;
    update_parameters_dir(&args, &client)
        .await
        .context("Failed to create / update parameters directory")?;

    // Register as a DBus service:
    let dbus_builder_opt = match args.dbus_bus {
        PuppetDaemonDbusBus::Session => Some(zbus::connection::Builder::session()?),
        PuppetDaemonDbusBus::System => Some(zbus::connection::Builder::system()?),
        PuppetDaemonDbusBus::None => None,
    };

    let _dbus_conn = if let Some(dbus_builder) = dbus_builder_opt {
        Some(
            dbus_builder
                .name("ci.treadmill.Puppet")?
                .serve_at(
                    "/ci/treadmill/Puppet",
                    DbusPuppet {
                        control_socket_client: client.clone(),
                    },
                )?
                .build()
                .await?,
        )
    } else {
        None
    };

    // Report the puppet as ready:
    client
        .report_ready()
        .await
        .context("Reporting puppet ready status to supervisor")?;

    info!("Puppet started, waiting for supervisor events. Exit with CTRL+C");
    sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
        .context("Notifying service manager that puppet is ready")?;

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

        debug!("Received supervisor event: {:?}", &event);

        match event {
            SupervisorEvent::SSHKeysUpdated => {
                update_authorized_keys_wrapper(&args, &client).await?;
            }

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

async fn handle_job_command(job_args: &PuppetJobCommand, proxy: DbusPuppetProxy<'_>) -> Result<()> {
    match job_args.job_command {
        PuppetJobSubcommands::Terminate(PuppetJobTerminateCommand) => {
            info!("Requesting job termination.");
            proxy
                .terminate_job()
                .await
                .context("Requesting job termination")
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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

    let args = PuppetCli::parse();

    async fn client_dbus_connect(bus_options: &ClientBusOptions) -> Result<DbusPuppetProxy> {
        let conn = match bus_options.dbus_bus {
            PuppetDbusBus::System => zbus::Connection::system().await?,
            PuppetDbusBus::Session => zbus::Connection::session().await?,
        };

        let proxy = DbusPuppetProxy::new(&conn).await?;

        Ok(proxy)
    }

    match args.puppet_command {
        PuppetCommands::Daemon(daemon_args) => daemon_main(daemon_args).await,
        PuppetCommands::Job(job_args) => {
            let proxy = client_dbus_connect(&job_args.bus_options).await?;
            handle_job_command(&job_args, proxy).await
        }
    }
}
