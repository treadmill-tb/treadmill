use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::str::FromStr;
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
    Set {
        /// Job UUID or ^<hex> suffix
        job: JobRef,
    },
    /// Clear the active job
    Clear,
}

#[derive(Subcommand, Debug)]
pub enum JobCommand {
    /// Request a job to be terminated
    Terminate {
        /// Target job; defaults to the active job, or inside a job to that job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
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
    /// Open an interactive SSH session to a job
    Ssh {
        /// Target job; defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
        #[command(flatten)]
        target: SshJob,
        /// Positional arguments to pass to SSH (e.g. command to run), elide to
        /// request an interactive session
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command in a job via SSH and return its exit status
    Exec {
        /// Target job; defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
        #[command(flatten)]
        target: SshJob,
        /// The command and its arguments
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open an SFTP session to a job
    Sftp {
        /// Target job; defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
        #[command(flatten)]
        target: SshJob,
        /// Trailing sftp arguments, placed after the destination
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Copy a local file into a job via SFTP
    Upload {
        /// Target job; defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
        #[command(flatten)]
        target: SshJob,
        local: PathBuf,
        remote: Option<String>,
    },
    /// Copy a file out of a job via SFTP
    Download {
        /// Target job; defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
        #[command(flatten)]
        target: SshJob,
        remote: String,
        local: Option<PathBuf>,
    },
    /// Bridge stdio to a job service over a WebSocket, used for ssh's ProxyCommand
    WsProxy {
        /// Target job; defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<JobRef>,
        service: String,
    },
    /// Alias for `context job set`
    SetActive {
        /// Job UUID or ^<hex> suffix
        job: JobRef,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SshJob {
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

/// A job ID argument: a full UUID, or `^<hex>`, a suffix of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRef {
    Id(Uuid),
    Short(String),
}

impl JobRef {
    #[cfg(feature = "daemon")]
    pub fn matches(&self, id: Uuid) -> bool {
        match self {
            Self::Id(own) => *own == id,
            Self::Short(suffix) => id.simple().to_string().ends_with(suffix.as_str()),
        }
    }
}

impl FromStr for JobRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let Some(hex) = s.strip_prefix('^') else {
            return Uuid::parse_str(s)
                .map(Self::Id)
                .map_err(|e| format!("not a UUID or ^<hex> suffix: {e}"));
        };
        if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("expected ^ followed by 1 to 32 hex digits".to_string());
        }
        // ^ with all 32 digits is a full UUID.
        Ok(Uuid::parse_str(hex).map_or_else(|_| Self::Short(hex.to_ascii_lowercase()), Self::Id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a6";

    #[test]
    fn a_job_ref_is_a_uuid_or_a_suffix() {
        let id = Uuid::parse_str(JOB).unwrap();
        assert_eq!(JOB.parse(), Ok(JobRef::Id(id)));
        assert_eq!(format!("^{}", id.simple()).parse(), Ok(JobRef::Id(id)));
        assert_eq!("^D3E4f5a6".parse(), Ok(JobRef::Short("d3e4f5a6".into())));
    }

    #[test]
    fn a_malformed_job_ref_is_refused() {
        for bad in [
            "",
            "^",
            "^xyz",
            "d3e4f5a6",
            "^-d3e4",
            &format!("^0{}", JOB.replace('-', "")),
        ] {
            assert!(bad.parse::<JobRef>().is_err(), "{bad:?}");
        }
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn a_short_id_matches_by_suffix() {
        let id = Uuid::parse_str(JOB).unwrap();
        assert!(JobRef::Short("f5a6".into()).matches(id));
        assert!(!JobRef::Short("f5a7".into()).matches(id));
        assert!(JobRef::Id(id).matches(id));
    }
}
