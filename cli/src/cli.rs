use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
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

#[derive(Args, Debug, Clone, Default)]
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
    Login(LoginArgs),
    /// Revoke and remove the stored credentials
    Logout,
    /// Show the current identity
    Whoami,
    /// Inspect or change local defaults
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// Create, inspect, and interact with Treadmill jobs
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },
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
    WsProxy { hostname: String, port: u16 },
    /// Alias for `context job set`
    SetActive { job: Uuid },
}

#[derive(Args, Debug, Clone)]
pub struct JobTarget {
    /// Target job; defaults to the active job
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
