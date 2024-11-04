//! Switchboard runner. Run as a command-line tool.

use clap::{Parser, Subcommand};
use treadmill_switchboard::serve::ServeCommand;

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
}
impl Command {
    async fn run(self) -> miette::Result<()> {
        match self {
            Command::Serve(serve_cmd) => treadmill_switchboard::serve::serve(serve_cmd).await,
        }
    }
}

#[tokio::main]
async fn main() {
    let cli_args = Args::parse();

    if let Err(e) = cli_args.command.run().await {
        eprintln!("Failed to run `serve` command:\n{e:?}");
    }
}
