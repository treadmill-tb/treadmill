use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

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
        #[arg(short = 'c', long = "cfg", env = "TML_CFG")]
        cfg: PathBuf,
    },
    /// Create a user (intended for debugging and testing purposes)
    CreateUser {
        #[arg(short = 'c', long = "cfg", env = "TML_CFG")]
        cfg: PathBuf,

        /// Username of the user to create
        username: String,
        /// Email of the user to create
        email: String,
        /// Password to use for this user
        password: String,
    },
    /// Create a token (intended for debugging and testing purposes)
    CreateToken {
        #[arg(short = 'c', long = "cfg", env = "TML_CFG")]
        cfg: PathBuf,

        /// The user to create the token under
        with_user_id: Uuid,
        /// How long the token should be valid for
        #[arg(value_parser = humantime_serde::re::humantime::parse_duration)]
        lifetime: Duration,
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
            if let Err(err) = tml_switchboard::server::serve(&cfg).await {
                eprintln!("Failed to run command: serve, due to error:\n{:?}", err);
            }
        }
        Command::CreateUser {
            cfg,
            username,
            email,
            password,
        } => {
            if let Err(err) =
                tml_switchboard::tools::create_user(&cfg, username, email, password).await
            {
                eprintln!(
                    "Failed to run command: create-user, due to error:\n{:?}",
                    err
                );
            }
        }
        Command::CreateToken {
            cfg,
            with_user_id,
            lifetime,
        } => {
            if let Err(err) =
                tml_switchboard::tools::create_token(&cfg, with_user_id, lifetime).await
            {
                eprintln!(
                    "Failed to run command: create-token, due to error:\n{:?}",
                    err
                );
            }
        }
    }
}
