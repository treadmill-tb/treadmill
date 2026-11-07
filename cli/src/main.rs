mod cli;
#[cfg(feature = "user")]
mod context;
#[cfg(feature = "user")]
mod ctx;
#[cfg(feature = "daemon")]
mod daemon;
#[cfg_attr(not(feature = "user"), allow(dead_code))]
mod diag;
#[cfg_attr(not(feature = "user"), allow(dead_code))]
mod display;
#[cfg_attr(not(feature = "user"), allow(dead_code))]
mod event;
#[cfg(feature = "user")]
mod host;
#[cfg(feature = "user")]
mod job;
#[cfg(feature = "user")]
mod listing;
#[cfg(feature = "user")]
mod login;
#[cfg(feature = "user")]
mod ssh;
#[cfg(feature = "user")]
mod sshconfig;
mod terminate;
#[cfg_attr(not(feature = "user"), allow(dead_code))]
mod tui;

#[cfg(feature = "daemon")]
use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
#[cfg(feature = "daemon")]
use treadmill_client::daemon as client_daemon;
use treadmill_client::error::{Cancelled, Hinted};
#[cfg(feature = "user")]
use treadmill_client::wsproxy;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use cli::{Cli, Command, Globals, JobCommand, JobRef};
#[cfg(feature = "daemon")]
use cli::{DaemonJobCommand, ExitOutcome};
#[cfg(feature = "user")]
use cli::{SshCommand, UserJobCommand};
#[cfg(feature = "user")]
use ctx::Ctx;
#[cfg(feature = "daemon")]
use treadmill_rs::api::switchboard::jobs::{JobExitStatusRequest, TaskExitStatus};

fn main() -> std::process::ExitCode {
    let args = Cli::parse();
    #[cfg(feature = "user")]
    ctx::set_color(args.globals.color);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(&anyhow::Error::new(e)),
    };

    match runtime.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(error: &anyhow::Error) -> std::process::ExitCode {
    let (error, hint) = match error.downcast_ref::<Hinted>() {
        Some(hinted) => (hinted.error(), Some(hinted.hint())),
        None => (error, None),
    };
    if let Some(io) = error.downcast_ref::<std::io::Error>()
        && io.kind() == std::io::ErrorKind::Interrupted
    {
        return std::process::ExitCode::from(130);
    }

    let code = if let Some(cancelled) = error.downcast_ref::<Cancelled>() {
        let style = anstyle::AnsiColor::Red.on_default();
        let dim = anstyle::Style::new().dimmed();
        anstream::eprintln!(
            "{style}✗{style:#} Cancelled {dim}· {}{dim:#}",
            cancelled.unchanged()
        );
        std::process::ExitCode::FAILURE
    } else {
        let style = anstyle::AnsiColor::Red.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!("{style}error:{style:#} {error:#}");
        match error.downcast_ref::<treadmill_client::error::ExitError>() {
            Some(exit) => std::process::ExitCode::from(exit.code()),
            None => std::process::ExitCode::FAILURE,
        }
    };
    if let Some(hint) = hint {
        let style = anstyle::AnsiColor::Cyan.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!("{style}hint:{style:#} {hint}");
    }
    code
}

async fn run(args: Cli) -> Result<()> {
    match args.command {
        #[cfg(feature = "user")]
        Command::Login(login_args) => {
            login::login(&mut Ctx::load(&args.globals)?, &login_args).await
        }
        #[cfg(feature = "user")]
        Command::Logout => login::logout(&mut Ctx::load(&args.globals)?).await,
        #[cfg(feature = "user")]
        Command::Whoami => login::whoami(&Ctx::load(&args.globals)?).await,
        #[cfg(feature = "user")]
        Command::Context { command } => {
            context::run(&mut Ctx::load(&args.globals)?, &command).await
        }
        Command::Job {
            command: JobCommand::Terminate { job, yes },
        } => terminate(&args.globals, job, yes).await,
        #[cfg(feature = "user")]
        Command::Job {
            command: JobCommand::User(command),
        } => user_job(&mut Ctx::load(&args.globals)?, &command).await,
        #[cfg(feature = "daemon")]
        Command::Job {
            command: JobCommand::Daemon(command),
        } => daemon_job(&args.globals, command).await,
        #[cfg(feature = "user")]
        Command::Host { command } => host::run(&Ctx::load(&args.globals)?, &command).await,
        #[cfg(feature = "user")]
        Command::Ssh { command } => {
            let mut ctx = Ctx::load(&args.globals)?;
            require_streaming(&ctx, true);
            match command {
                SshCommand::Setup(args) => sshconfig::setup(&ctx, &args).await,
                SshCommand::Proxy { host } => ssh::proxy(&mut ctx, &host).await,
            }
        }
        #[cfg(feature = "daemon")]
        Command::Daemon(daemon_args) => {
            daemon::run(daemon_args, args.globals.dbus_bus.into()).await
        }
    }
}

#[cfg(feature = "daemon")]
async fn daemon_job(globals: &Globals, command: DaemonJobCommand) -> Result<()> {
    match command {
        DaemonJobCommand::ReloadServices => {
            client_daemon::reload_services(globals.dbus_bus.into()).await
        }
        DaemonJobCommand::SetExitStatus { outcome, message } => {
            let credentials = client_daemon::credentials(globals.dbus_bus.into())
                .await?
                .context("reporting an exit status needs the tml daemon of a job")?;
            let outcome = match outcome {
                ExitOutcome::Success => TaskExitStatus::Success,
                ExitOutcome::Failure => TaskExitStatus::Failure,
            };
            credentials
                .client()
                .put_job_exit_status(
                    credentials.job_id,
                    &JobExitStatusRequest { outcome, message },
                )
                .await
                .context("reporting the job's exit status")?;
            Ok(())
        }
    }
}

async fn job_client(globals: &Globals, job: Option<JobRef>) -> Result<(SwitchboardClient, Uuid)> {
    #[cfg(feature = "daemon")]
    if let Some(credentials) = client_daemon::credentials(globals.dbus_bus.into()).await? {
        let job_id = credentials.check_job(job.as_ref())?;
        return Ok((credentials.client(), job_id));
    }

    #[cfg(feature = "user")]
    {
        let ctx = Ctx::load(globals)?;
        Ok((
            ctx.authenticated_client()?,
            ctx.resolve_job(job.as_ref()).await?,
        ))
    }
    #[cfg(not(feature = "user"))]
    anyhow::bail!("no tml daemon found on D-Bus")
}

async fn terminate(globals: &Globals, job: Option<JobRef>, yes: bool) -> Result<()> {
    let (client, job_id) = job_client(globals, job).await?;
    terminate::run(globals, &client, job_id, yes).await
}

#[cfg(feature = "user")]
fn require_streaming(ctx: &Ctx, streams: bool) {
    if ctx.human() || !streams {
        return;
    }
    let style = anstyle::AnsiColor::Red.on_default() | anstyle::Effects::BOLD;
    anstream::eprintln!(
        "{style}error:{style:#} -o json and -o jsonl are not supported here, because this \
         command's output is a byte stream"
    );
    std::process::exit(2);
}

#[cfg(feature = "user")]
async fn user_job(ctx: &mut Ctx, command: &UserJobCommand) -> Result<()> {
    // The SSH family hands the terminal to another program, so it has no
    // structured rendering to offer.
    require_streaming(
        ctx,
        matches!(
            command,
            UserJobCommand::Ssh { .. }
                | UserJobCommand::Exec { .. }
                | UserJobCommand::Sftp { .. }
                | UserJobCommand::Upload { .. }
                | UserJobCommand::Download { .. }
                | UserJobCommand::WsProxy { .. }
        ),
    );

    match command {
        UserJobCommand::Create(args) => job::create(ctx, args).await,
        UserJobCommand::Wait { job } => job::wait::run(ctx, job.as_ref()).await,
        UserJobCommand::Status { job } => job::status::run(ctx, job.as_ref()).await,
        UserJobCommand::Ssh { job, target, args } => {
            ssh::ssh(ctx, ctx.resolve_job(job.as_ref()).await?, target, args).await
        }
        UserJobCommand::Exec {
            job,
            target,
            command,
        } => ssh::exec_command(ctx, ctx.resolve_job(job.as_ref()).await?, target, command).await,
        UserJobCommand::Sftp { job, target, args } => {
            ssh::sftp(ctx, ctx.resolve_job(job.as_ref()).await?, target, args).await
        }
        UserJobCommand::Upload {
            job,
            target,
            local,
            remote,
        } => {
            ssh::upload(
                ctx,
                ctx.resolve_job(job.as_ref()).await?,
                target,
                local,
                remote.as_deref(),
            )
            .await
        }
        UserJobCommand::Download {
            job,
            target,
            remote,
            local,
        } => {
            ssh::download(
                ctx,
                ctx.resolve_job(job.as_ref()).await?,
                target,
                remote,
                local.as_deref(),
            )
            .await
        }
        UserJobCommand::SetActive { job } => {
            let job = ctx.resolve_job(Some(job)).await?;
            context::set_active(ctx, job)
        }
        UserJobCommand::WsProxy {
            job,
            listen_addr,
            service,
        } => {
            let job_id = ctx.resolve_job(job.as_ref()).await?;
            match listen_addr {
                Some(addr) => {
                    let listener = tokio::net::TcpListener::bind(*addr).await?;
                    loop {
                        let (stream, _) = listener.accept().await?;
                        let (infd, outfd) = stream.into_split();
                        wsproxy::run(&mut ctx.session, job_id, infd, outfd, service, None).await?;
                    }
                }
                None => {
                    let (infd, outfd) = (tokio::io::stdin(), tokio::io::stdout());
                    wsproxy::run(&mut ctx.session, job_id, infd, outfd, service, None).await
                }
            }
        }
        UserJobCommand::List(args) => job::list::run(ctx, args).await,
        UserJobCommand::Update(args) => job::update(ctx, args).await,
        UserJobCommand::Lease {
            job,
            length,
            until,
            if_revision,
            yes,
        } => {
            let lease = match (length, until) {
                (Some(length), _) => *length,
                (None, Some(until)) => job::resolve_lease_until(*until),
                (None, None) => unreachable!("clap requires a LENGTH or --until"),
            };
            job::lease(ctx, job.as_ref(), lease, *if_revision, *yes).await
        }
    }
}
