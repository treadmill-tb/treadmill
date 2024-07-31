//! The actual switchboard runner. Run as a command-line tool.

use clap::{Parser, Subcommand};
use tml_switchboard::server::ServeCommand;
use tml_switchboard::tools::{CreateTokenCommand, CreateUserCommand};

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
    Serve(ServeCommand),
    /// Create a user (intended for debugging and testing purposes)
    CreateUser(CreateUserCommand),
    /// Create a token (intended for debugging and testing purposes)
    CreateToken(CreateTokenCommand),
}

#[tokio::main]
async fn main() {
    let cli_args = Args::parse();
    match cli_args.command {
        Command::Serve(cmd) => {
            if let Err(err) = tml_switchboard::server::serve(cmd).await {
                eprintln!("Failed to run command: serve, due to error:\n{:?}", err);
            }
        }
        Command::CreateUser(cmd) => {
            if let Err(err) = tml_switchboard::tools::create_user(cmd).await {
                eprintln!(
                    "Failed to run command: create-user, due to error:\n{:?}",
                    err
                );
            }
        }
        Command::CreateToken(cmd) => {
            if let Err(err) = tml_switchboard::tools::create_token(cmd).await {
                eprintln!(
                    "Failed to run command: create-token, due to error:\n{:?}",
                    err
                );
            }
        }
    }
}
