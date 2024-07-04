//! Coordinator server.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod cfg;
mod server;

/// Arguments passed to the switchboard from the command line
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Debug, Subcommand)]
#[command(about)]
pub enum Command {
    /// Run the switchboard server.
    Serve {
        /// Path to server configuration file
        cfg: PathBuf,
    },
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build Tokio runtime, fatal")
        .block_on(async { async_main().await });
}

async fn async_main() {
    let cli_args = Args::parse();
    match cli_args.command {
        Command::Serve { cfg } => {
            if let Err(err) = server::serve(&cfg).await {
                eprintln!("Failed to run command: serve, due to error:\n{:?}", err);
            }
        }
    }
}
