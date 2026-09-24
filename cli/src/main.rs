mod cli;
#[cfg(feature = "user")]
mod config;
#[cfg(feature = "user")]
mod context;
#[cfg(feature = "user")]
mod ctx;
#[cfg(feature = "daemon")]
mod daemon;
#[cfg(feature = "user")]
mod login;
#[cfg(feature = "user")]
mod ssh;
#[cfg(feature = "user")]
mod sshconfig;
#[cfg(feature = "user")]
mod state;
#[cfg(feature = "user")]
mod wsproxy;

use anyhow::Result;
use clap::Parser;

#[cfg(feature = "daemon")]
use cli::DaemonJobCommand;
use cli::{Cli, Command, JobCommand};
#[cfg(feature = "user")]
use cli::{SshCommand, UserJobCommand};
#[cfg(feature = "user")]
use ctx::Ctx;

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
    if let Some(io) = error.downcast_ref::<std::io::Error>()
        && io.kind() == std::io::ErrorKind::Interrupted
    {
        return std::process::ExitCode::from(130);
    }

    let style = anstyle::AnsiColor::Red.on_default() | anstyle::Effects::BOLD;
    anstream::eprintln!("{style}error:{style:#} {error:#}");
    std::process::ExitCode::FAILURE
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
        #[cfg(feature = "user")]
        Command::Job {
            command: JobCommand::User(command),
        } => user_job(&mut Ctx::load(&args.globals)?, &command).await,
        #[cfg(feature = "daemon")]
        Command::Job {
            command: JobCommand::Daemon(command),
        } => daemon_job(command).await,
        #[cfg(feature = "user")]
        Command::Ssh { command } => {
            let mut ctx = Ctx::load(&args.globals)?;
            require_streaming(&ctx, true);
            match command {
                SshCommand::Setup(args) => sshconfig::setup(&ctx, &args),
                SshCommand::Proxy { host } => ssh::proxy(&mut ctx, &host).await,
            }
        }
        #[cfg(feature = "daemon")]
        Command::Daemon(daemon_args) => daemon::run(daemon_args).await,
    }
}

#[cfg(feature = "daemon")]
async fn daemon_job(command: DaemonJobCommand) -> Result<()> {
    match command {
        DaemonJobCommand::Terminate { bus } => daemon::terminate_job(&bus).await,
        DaemonJobCommand::ReloadServices { bus } => daemon::reload_services(&bus).await,
    }
}

#[cfg(feature = "user")]
fn require_streaming(ctx: &Ctx, streams: bool) {
    if ctx.human() || !streams {
        return;
    }
    let style = anstyle::AnsiColor::Red.on_default() | anstyle::Effects::BOLD;
    anstream::eprintln!(
        "{style}error:{style:#} this command streams bytes and has no structured output"
    );
    std::process::exit(2);
}

#[cfg(feature = "user")]
async fn user_job(ctx: &mut Ctx, command: &UserJobCommand) -> Result<()> {
    // The SSH family hands the terminal to another program, so it has no
    // structured rendering to offer.
    require_streaming(ctx, !matches!(command, UserJobCommand::SetActive { .. }));

    match command {
        UserJobCommand::Ssh { target, args } => ssh::ssh(ctx, target, args).await,
        UserJobCommand::Exec { target, command } => ssh::exec_command(ctx, target, command).await,
        UserJobCommand::Sftp { target, args } => ssh::sftp(ctx, target, args).await,
        UserJobCommand::Upload {
            target,
            local,
            remote,
        } => ssh::upload(ctx, target, local, remote.as_deref()).await,
        UserJobCommand::Download {
            target,
            remote,
            local,
        } => ssh::download(ctx, target, remote, local.as_deref()).await,
        UserJobCommand::SetActive { job } => context::set_active(ctx, *job),
        UserJobCommand::WsProxy { job, service } => wsproxy::run(ctx, *job, service, None).await,
    }
}
