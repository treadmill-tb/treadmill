mod cli;
mod config;
mod context;
mod ctx;
mod login;
mod ssh;
mod sshconfig;
mod state;
mod wsproxy;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, JobCommand, SshCommand};
use ctx::Ctx;

fn main() -> std::process::ExitCode {
    let args = Cli::parse();
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
    let mut ctx = Ctx::load(&args.globals)?;

    match &args.command {
        Command::Login(login_args) => login::login(&mut ctx, login_args).await,
        Command::Logout => login::logout(&mut ctx).await,
        Command::Whoami => login::whoami(&ctx).await,
        Command::Context { command } => context::run(&mut ctx, command).await,
        Command::Job { command } => job(&mut ctx, command).await,
        Command::Ssh { command } => {
            require_streaming(&ctx, true);
            match command {
                SshCommand::Setup(args) => sshconfig::setup(&ctx, args),
                SshCommand::Proxy { host } => ssh::proxy(&mut ctx, host).await,
            }
        }
    }
}

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

async fn job(ctx: &mut Ctx, command: &JobCommand) -> Result<()> {
    // The SSH family hands the terminal to another program, so it has no
    // structured rendering to offer.
    require_streaming(ctx, !matches!(command, JobCommand::SetActive { .. }));

    match command {
        JobCommand::Ssh { target, args } => ssh::ssh(ctx, target, args).await,
        JobCommand::Exec { target, command } => ssh::exec_command(ctx, target, command).await,
        JobCommand::Sftp { target, args } => ssh::sftp(ctx, target, args).await,
        JobCommand::Upload {
            target,
            local,
            remote,
        } => ssh::upload(ctx, target, local, remote.as_deref()).await,
        JobCommand::Download {
            target,
            remote,
            local,
        } => ssh::download(ctx, target, remote, local.as_deref()).await,
        JobCommand::SetActive { job } => context::set_active(ctx, *job),
        JobCommand::WsProxy { job, service } => wsproxy::run(ctx, *job, service, None).await,
    }
}
