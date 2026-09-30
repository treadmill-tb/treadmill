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
        /// Terminate without asking for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
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
    /// List the jobs visible to you
    List(JobListArgs),
    /// Change a job's label, lease or annotations
    ///
    /// All changes apply together or not at all. A lease change that would
    /// leave less than five minutes asks for confirmation first. Exits with 3
    /// if `--if-revision` names a revision the job is no longer at, and with 4
    /// if the switchboard refuses the lease change (e.g. the job is stopping).
    Update(JobUpdateArgs),
    /// Change a job's lease; shorthand for `update --lease`/`--lease-until`
    ///
    /// A change that would leave less than five minutes asks for confirmation
    /// first. Exits with 3 if `--if-revision` names a revision the job is no
    /// longer at, and with 4 if the switchboard refuses the lease change.
    #[command(group = clap::ArgGroup::new("new_lease").required(true))]
    Lease {
        /// Target job (UUID or `^<hex>` short id); defaults to the active job
        #[arg(long, value_name = "JOB")]
        job: Option<String>,
        /// The lease's length from the job's start (`30m`), or a change to it
        /// (`+30m`, `-10m`)
        #[arg(
            value_name = "LENGTH",
            group = "new_lease",
            value_parser = parse_lease_length,
            allow_hyphen_values = true
        )]
        length: Option<LeaseSpec>,
        /// When the lease ends instead: a time (RFC 3339), or relative to now
        /// (`now`, `now+20m`, `now-5m`)
        #[arg(long, value_name = "TIME", group = "new_lease", value_parser = parse_lease_until)]
        until: Option<LeaseUntil>,
        /// Only apply if the job is still at this revision
        #[arg(long, value_name = "REVISION")]
        if_revision: Option<i64>,
        /// Don't ask before leaving the lease (almost) expired
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[cfg(feature = "user")]
#[derive(Args, Debug)]
#[command(group = clap::ArgGroup::new("section").args(["active", "reclaimable", "finished"]))]
pub struct JobListArgs {
    /// Only jobs matching all these terms: `^<hex>` for an id suffix, anything
    /// else for the label, image, host or owner name
    #[arg(value_name = "TERMS")]
    pub terms: Vec<String>,
    /// Include jobs you own
    #[arg(long)]
    pub mine: bool,
    /// Include jobs owned by one of your groups
    #[arg(long)]
    pub groups: bool,
    /// Include jobs shared with you or your groups
    #[arg(long)]
    pub shared: bool,
    /// Include every job (admins only)
    #[arg(long)]
    pub global: bool,
    /// List only running jobs with a lease left
    #[arg(long)]
    pub active: bool,
    /// List only running jobs whose host may be reclaimed for another job
    #[arg(long)]
    pub reclaimable: bool,
    /// List finished jobs
    #[arg(long)]
    pub finished: bool,
    /// Jobs per page; without a pager, the most jobs listed per section
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: usize,
    /// Continue a previous listing from its cursor
    #[arg(long, value_name = "CURSOR", requires = "section")]
    pub cursor: Option<String>,
    /// Refresh the list until quit
    #[arg(long, conflicts_with_all = ["finished", "cursor"])]
    pub watch: bool,
    /// Print only the first `--limit` jobs per section, without a pager
    #[arg(long)]
    pub no_pager: bool,
}

#[cfg(feature = "user")]
#[derive(Args, Debug)]
pub struct JobUpdateArgs {
    /// Target job (UUID or `^<hex>` short id); defaults to the active job
    #[arg(long, value_name = "JOB")]
    pub job: Option<String>,
    /// Set the display label
    #[arg(long, value_name = "LABEL", conflicts_with = "clear_label")]
    pub label: Option<String>,
    /// Remove the display label
    #[arg(long)]
    pub clear_label: bool,
    /// The lease's length from the job's start (`30m`), or a change to it
    /// (`+30m`, `-10m`)
    #[arg(
        long,
        value_name = "LENGTH",
        conflicts_with = "lease_until",
        value_parser = parse_lease_length,
        allow_hyphen_values = true
    )]
    pub lease: Option<LeaseSpec>,
    /// When the lease ends: a time (RFC 3339), or relative to now (`now`,
    /// `now+20m`, `now-5m`)
    #[arg(long, value_name = "TIME", value_parser = parse_lease_until)]
    pub lease_until: Option<LeaseUntil>,
    /// What happens when the lease expires
    #[arg(long, value_enum, value_name = "ACTION")]
    pub expiry_action: Option<ExpiryAction>,
    /// Set an annotation, repeatable
    #[arg(long = "annotation", value_name = "KEY=VALUE", value_parser = parse_param)]
    pub annotations: Vec<(String, String)>,
    /// Remove an annotation, repeatable
    #[arg(long = "remove-annotation", value_name = "KEY")]
    pub remove_annotations: Vec<String>,
    /// Only apply if the job is still at this revision
    #[arg(long, value_name = "REVISION")]
    pub if_revision: Option<i64>,
    /// Don't ask before leaving the lease (almost) expired
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// What happens when a job's lease expires.
#[cfg(feature = "user")]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryAction {
    /// The job is stopped
    Terminate,
    /// The job runs on, but its host may be reclaimed for another job
    Preempt,
}

/// When a lease should end, as given on the command line. One relative to now
/// is resolved against this machine's clock when the request is sent.
#[cfg(feature = "user")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseUntil {
    At(chrono::DateTime<chrono::Utc>),
    FromNow(chrono::TimeDelta),
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

/// A lease length or a change to it: `30m`, `+30m`, `-10m`.
#[cfg(feature = "user")]
fn parse_lease_length(s: &str) -> Result<LeaseSpec, String> {
    let end_time = || {
        format!("{s:?} is an end time; pass it with `--lease-until` (`--until` for `job lease`)")
    };
    if s.starts_with("now") {
        return Err(end_time());
    }
    match s.parse()? {
        LeaseSpec::ExpiresAt(_) => Err(end_time()),
        spec => Ok(spec),
    }
}

/// A lease end: an RFC 3339 time, `now`, `now+DURATION` or `now-DURATION`.
#[cfg(feature = "user")]
fn parse_lease_until(s: &str) -> Result<LeaseUntil, String> {
    if let Some(offset) = s.strip_prefix("now") {
        if offset.is_empty() {
            return Ok(LeaseUntil::FromNow(chrono::TimeDelta::zero()));
        }
        return match offset.parse() {
            Ok(LeaseSpec::Adjust(offset)) => Ok(LeaseUntil::FromNow(offset)),
            _ => Err(format!(
                "{s:?} is not of the form `now`, `now+DURATION` or `now-DURATION`"
            )),
        };
    }
    match s.parse() {
        Ok(LeaseSpec::ExpiresAt(at)) => Ok(LeaseUntil::At(at)),
        Ok(_) => Err(format!(
            "{s:?} is a lease length; pass it with `--lease` (as `job lease {s}`), or \
             write `now+{}` for an end relative to now",
            s.trim_start_matches(['+', '-'])
        )),
        Err(_) => Err(format!(
            "{s:?} is not an RFC 3339 time, `now`, `now+DURATION` or `now-DURATION`"
        )),
    }
}

#[cfg(feature = "user")]
fn parse_param(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .ok_or_else(|| format!("{s:?} is not of the form KEY=VALUE"))
}

#[cfg(all(test, feature = "user"))]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn lease_lengths_are_durations_or_changes_to_them() {
        assert_eq!(
            parse_lease_length("30m"),
            Ok(LeaseSpec::Set(TimeDelta::minutes(30)))
        );
        assert_eq!(
            parse_lease_length("+30m"),
            Ok(LeaseSpec::Adjust(TimeDelta::minutes(30)))
        );
        assert_eq!(
            parse_lease_length("-10m"),
            Ok(LeaseSpec::Adjust(TimeDelta::minutes(-10)))
        );
        for end in ["2026-08-28T14:00:00Z", "now", "now+20m"] {
            let err = parse_lease_length(end).unwrap_err();
            assert!(err.contains("--lease-until"), "{end}: {err}");
        }
    }

    #[test]
    fn lease_ends_are_times_or_relative_to_now() {
        assert!(matches!(
            parse_lease_until("2026-08-28T14:00:00Z"),
            Ok(LeaseUntil::At(_))
        ));
        assert_eq!(
            parse_lease_until("now"),
            Ok(LeaseUntil::FromNow(TimeDelta::zero()))
        );
        assert_eq!(
            parse_lease_until("now+20m"),
            Ok(LeaseUntil::FromNow(TimeDelta::minutes(20)))
        );
        assert_eq!(
            parse_lease_until("now-5m"),
            Ok(LeaseUntil::FromNow(TimeDelta::minutes(-5)))
        );
        let err = parse_lease_until("20m").unwrap_err();
        assert!(err.contains("--lease") && err.contains("now+20m"), "{err}");
        assert!(parse_lease_until("now20m").is_err());
        assert!(parse_lease_until("nowish").is_err());
        assert!(parse_lease_until("soon").is_err());
    }

    #[test]
    fn job_commands_parse() {
        Cli::try_parse_from(["tml", "job", "lease", "-10m", "--job", "x"]).unwrap();
        Cli::try_parse_from(["tml", "job", "lease", "--until", "now+20m", "-y"]).unwrap();
        assert!(Cli::try_parse_from(["tml", "job", "lease"]).is_err());
        assert!(Cli::try_parse_from(["tml", "job", "lease", "30m", "--until", "now"]).is_err());
        Cli::try_parse_from([
            "tml",
            "job",
            "update",
            "--lease-until",
            "now+20m",
            "--annotation",
            "a.b=c=d",
            "--remove-annotation",
            "e",
            "--if-revision",
            "3",
            "--yes",
        ])
        .unwrap();
        assert!(
            Cli::try_parse_from([
                "tml",
                "job",
                "update",
                "--lease",
                "30m",
                "--lease-until",
                "now"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["tml", "job", "update", "--label", "x", "--clear-label"]).is_err()
        );
        Cli::try_parse_from(["tml", "job", "list", "--mine", "--finished", "-o", "json"]).unwrap();
    }
}
