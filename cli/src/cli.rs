use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
#[cfg(feature = "user")]
use treadmill_rs::api::switchboard::jobs::LeaseSpec;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(
    name = "tml",
    version,
    about = "CLI client for the Treadmill Distributed Hardware Testbed"
)]
pub struct Cli {
    #[command(flatten)]
    pub globals: Globals,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Global Options")]
pub struct Globals {
    /// Switchboard API URL
    #[arg(long, value_name = "URL", global = true)]
    pub switchboard: Option<String>,

    /// Configuration profile
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,

    /// Configuration file
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Output format
    #[arg(short, long, value_enum, value_name = "FORMAT", global = true)]
    pub output: Option<OutputFormat>,

    /// Whether to colorize output
    #[arg(long, value_enum, value_name = "COLOR", global = true)]
    pub color: Option<ColorChoice>,

    /// Skip TLS certificate verification
    #[arg(long, global = true)]
    pub insecure_tls: bool,

    /// Suppress non-essential output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// The D-Bus the tml daemon is on
    #[cfg(feature = "daemon")]
    #[arg(
        long,
        value_enum,
        value_name = "BUS",
        default_value = "system",
        global = true
    )]
    pub dbus_bus: crate::daemon::DbusBus,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Authenticate with the switchboard
    #[cfg(feature = "user")]
    Login(LoginArgs),
    /// Revoke and remove the stored credentials
    #[cfg(feature = "user")]
    Logout,
    /// Show the current identity
    #[cfg(feature = "user")]
    Whoami,
    /// Inspect or change local defaults
    #[cfg(feature = "user")]
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// Create, inspect, and interact with Treadmill jobs
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Inspect hosts
    #[cfg(feature = "user")]
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Integrate Treadmill with the system SSH client
    #[cfg(feature = "user")]
    Ssh {
        #[command(subcommand)]
        command: SshCommand,
    },
    /// Run the in-image daemon, connecting to the supervisor's daemon API and
    /// serving D-Bus
    #[cfg(feature = "daemon")]
    Daemon(crate::daemon::DaemonArgs),
}

#[cfg(feature = "user")]
#[derive(Subcommand, Debug)]
pub enum HostCommand {
    /// List the hosts visible to you
    List,
    /// Show a host, including its full spec
    Show {
        /// Host UUID or name
        host: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SshCommand {
    /// Write the tml-managed SSH configuration and hook it into the user's SSH config
    Setup(SshSetupArgs),
    /// Bridge stdio to the job named by an SSH hostname, used for ProxyCommand
    #[command(hide = true)]
    Proxy { host: String },
}

#[derive(Args, Debug)]
pub struct SshSetupArgs {
    /// Edit the user's SSH configuration without asking for confirmation
    #[arg(long, short = 'y', conflicts_with = "print")]
    pub yes: bool,

    /// Only print the snippet, leaving the user's SSH configuration untouched
    #[arg(long)]
    pub print: bool,
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Identity provider to use
    #[arg(long, value_name = "NAME")]
    pub provider: Option<String>,

    /// Development-only mock identity to sign in as
    #[arg(long, value_name = "KEY", conflicts_with = "provider")]
    pub identity: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ContextCommand {
    /// Show the selected profile's defaults
    Show,
    /// Inspect or change the active job
    Job {
        #[command(subcommand)]
        command: ContextJobCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum ContextJobCommand {
    /// Set the active job
    Set { job: Uuid },
    /// Clear the active job
    Clear,
}

#[derive(Subcommand, Debug)]
pub enum JobCommand {
    /// Request a job to be terminated
    Terminate {
        /// Target job (UUID or `^<hex>` short id); defaults to the active job, or inside a job to that job
        #[arg(long, value_name = "JOB")]
        job: Option<String>,
    },
    #[cfg(feature = "user")]
    #[command(flatten)]
    User(UserJobCommand),
    #[cfg(feature = "daemon")]
    #[command(flatten)]
    Daemon(DaemonJobCommand),
}

#[cfg(feature = "daemon")]
#[derive(Subcommand, Debug)]
pub enum DaemonJobCommand {
    /// Rescan the service directory and announce the job's services
    ReloadServices,
    /// Report the outcome of the job this daemon runs in
    SetExitStatus {
        /// Whether the workload succeeded
        outcome: ExitOutcome,
        /// A note on the outcome, shown with the job
        message: Option<String>,
    },
}

#[cfg(feature = "daemon")]
#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum ExitOutcome {
    Success,
    Failure,
}

#[cfg(feature = "user")]
#[derive(Subcommand, Debug)]
pub enum UserJobCommand {
    /// Enqueue a new job
    ///
    /// Without `--image` or `--resume`, asks for the missing options in a
    /// terminal.
    Create(JobCreateArgs),
    /// Show a job's state, host, lease and services
    #[command(visible_alias = "show")]
    Status {
        /// Target job (UUID or `^<hex>` short id); defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<String>,
    },
    /// Wait until a job is ready and reachable over SSH
    Wait {
        /// Target job (UUID or `^<hex>` short id); defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<String>,
    },
    /// Open an interactive SSH session to a job
    Ssh {
        #[command(flatten)]
        target: JobTarget,
        /// Positional arguments to pass to SSH (e.g. command to run), elide to
        /// request an interactive session
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command in a job via SSH and return its exit status
    Exec {
        #[command(flatten)]
        target: JobTarget,
        /// The command and its arguments
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open an SFTP session to a job
    Sftp {
        #[command(flatten)]
        target: JobTarget,
        /// Trailing sftp arguments, placed after the destination
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Copy a local file into a job via SFTP
    Upload {
        #[command(flatten)]
        target: JobTarget,
        local: PathBuf,
        remote: Option<String>,
    },
    /// Copy a file out of a job via SFTP
    Download {
        #[command(flatten)]
        target: JobTarget,
        remote: String,
        local: Option<PathBuf>,
    },
    /// Bridge stdio to a job service over a WebSocket, used for ssh's ProxyCommand
    #[command(hide = true)]
    WsProxy { job: Uuid, service: String },
    /// Alias for `context job set`
    SetActive { job: Uuid },
}

#[derive(Args, Debug, Clone)]
pub struct JobTarget {
    /// Target job (UUID or `^<hex>` short id); defaults to the active job
    #[arg(long, value_name = "JOB")]
    pub job: Option<String>,

    /// Announced service to connect to
    #[arg(long, value_name = "NAME")]
    pub service: Option<String>,

    /// Remote SSH user
    #[arg(short = 'l', long, value_name = "USER")]
    pub user: Option<String>,

    /// SSH subprocess arguments, inserted before the destination user/IP
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    pub ssh_opt: Vec<String>,
}

#[cfg(feature = "user")]
#[derive(Args, Debug, Clone)]
pub struct JobCreateArgs {
    /// Image to run: an image set, by UUID or canonical name
    #[arg(long, value_name = "IMAGE")]
    pub image: Option<String>,

    /// Resume a terminated job on its host, from its working directory
    #[arg(long, value_name = "JOB", conflicts_with_all = ["image", "host"])]
    pub resume: Option<Uuid>,

    /// CEL predicate a host must satisfy, with its spec bound as `host`;
    /// defaults to any host
    #[arg(long, value_name = "CEL")]
    pub host: Option<String>,

    /// Display label
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,

    /// Lease duration (e.g. `30m`); defaults to the switchboard's default
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub lease: Option<chrono::TimeDelta>,

    /// Keep the job running once its lease expires, until its host is needed
    /// for another job
    #[arg(long)]
    pub reclaimable: bool,

    /// Annotation, repeatable
    #[arg(long = "annotation", value_name = "KEY=VALUE", value_parser = parse_param)]
    pub annotations: Vec<(String, String)>,

    /// Job parameter, repeatable
    #[arg(long = "param", value_name = "KEY=VALUE", value_parser = parse_param)]
    pub params: Vec<(String, String)>,

    /// Return once enqueued, without waiting until the job is ready
    #[arg(long)]
    pub no_wait: bool,

    /// Leave the active job unchanged
    #[arg(long)]
    pub no_set_active: bool,

    /// Never ask for missing options
    #[arg(long)]
    pub non_interactive: bool,
}

#[cfg(feature = "user")]
pub fn parse_duration(s: &str) -> Result<chrono::TimeDelta, String> {
    match s.parse()? {
        LeaseSpec::Set(duration) => Ok(duration),
        _ => Err(format!("{s:?} is not a plain duration")),
    }
}

#[cfg(feature = "user")]
fn parse_param(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .ok_or_else(|| format!("{s:?} is not of the form KEY=VALUE"))
}
