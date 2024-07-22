//! Backend for the `swx create-token` command.
//!
//! Creates an API token owned by a specified user with a specified lifetime.

use crate::server::token::ApiToken;
use chrono::Utc;
use miette::{Context, IntoDiagnostic, Result};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::path::PathBuf;
use std::time::Duration;
use treadmill_rs::config::{self};
use uuid::Uuid;

#[derive(Debug, clap::Parser)]
pub struct CreateTokenCommand {
    /// Path to the configuration file
    #[clap(long, env = "TREADMILL_CONFIG")]
    config: Option<PathBuf>,

    /// The user to create the token under
    created_by_user_id: Uuid,

    /// How long the token should be valid for
    #[clap(value_parser = humantime::parse_duration)]
    lifetime: Duration,
}

/// Create an API token with user `created_by_user_id` that will expire after `lifetime`, and print
/// the newly created token's information to standard output. Database connection information will
/// be taken from the configuration file.
pub async fn create_token(cmd: CreateTokenCommand) -> Result<()> {
    let config = config::load_config()
        .into_diagnostic()
        .wrap_err("Failed to load configuration")?;

    let pg_options = PgConnectOptions::new()
        .host(&config.switchboard.database.address)
        .port(config.switchboard.database.port)
        .database(&config.switchboard.database.name);
    //  .ssl_mode(PgSslMode::VerifyFull)
    /* TODO: supply ssl client cert */

    let pg_options = match &config.switchboard.database.auth {
        config::DatabaseAuth::PasswordAuth(config::PasswordAuth {
            ref username,
            ref password,
        }) => pg_options.username(username).password(password),
    };

    let pg_pool = PgPool::connect_with(pg_options)
        .await
        .into_diagnostic()
        .wrap_err("Failed to connect to database")?;

    let uuid = Uuid::new_v4();
    let token = ApiToken::generate();

    sqlx::query!(
        r#"insert into api_tokens values ($1, $2, $3, null, $4, $5, $6, $7);"#,
        uuid,
        token.as_bytes(),
        cmd.created_by_user_id,
        Utc::now(),
        Utc::now() + chrono::Duration::from_std(cmd.lifetime).unwrap(),
        &[],
        &[]
    )
    .execute(&pg_pool)
    .await
    .into_diagnostic()?;

    println!("token_id={uuid}");
    println!("token={token}");

    Ok(())
}
