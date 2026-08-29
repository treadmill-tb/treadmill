//! Switchboard runner. Run as a command-line tool.

use clap::{Parser, Subcommand};
use std::process::ExitCode;
use treadmill_switchboard::{manage::ManageCommand, routes::openapi_spec, serve::ServeCommand};

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
#[command(about)]
pub enum Command {
    Serve(ServeCommand),
    Manage(ManageCommand),
    GenerateOpenAPISpec,
}

impl Command {
    async fn run(self) -> anyhow::Result<()> {
        match self {
            Command::Serve(serve_cmd) => treadmill_switchboard::serve::serve(serve_cmd).await,
            Command::Manage(manage_cmd) => manage_cmd.run().await,
            Command::GenerateOpenAPISpec => {
                println!("{}", serde_norway::to_string(&openapi_spec()).unwrap());
                Ok(())
            }
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli_args = Args::parse();

    if let Err(e) = cli_args.command.run().await {
        eprintln!("Failed to run command:\n{e:?}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
