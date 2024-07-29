//! Backend for the `swx create-token` command.
//!
//! Creates an API token owned by a specified user with a specified lifetime.

use crate::cfg::{Config, DatabaseAuth, PasswordAuth};
use crate::server::token::ApiToken;
use chrono::Utc;
use miette::{Context, IntoDiagnostic};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, clap::Args)]
pub struct CreateTokenCommand {
    #[arg(short = 'c', long = "cfg", env = "TML_CFG")]
    cfg: PathBuf,

    /// The user to create the token under
    user_id: Uuid,
    /// How long the token should be valid for
    #[arg(value_parser = humantime_serde::re::humantime::parse_duration)]
    lifetime: Duration,

    #[arg(long)]
    inherit_user_perms: bool,
}

/// Create an API token with user `created_by_user_id` that will expire after `lifetime`, and print
/// the newly created token's information to standard output. Database connection information will
/// be taken from the configuration file at `config_path`.
pub async fn create_token(
    CreateTokenCommand {
        cfg,
        user_id,
        lifetime,
        inherit_user_perms,
    }: CreateTokenCommand,
) -> miette::Result<()> {
    let cfg_text = std::fs::read_to_string(&cfg)
        .into_diagnostic()
        .wrap_err("Failed to open configuration file")?;
    let cfg: Config = toml::from_str(&cfg_text)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to parse configuration file {}", cfg.display()))?;
    let pg_options = PgConnectOptions::new()
        .host(&cfg.database.address)
        .port(cfg.database.port)
        .database(&cfg.database.name)
        /*
            .ssl_mode(PgSslMode::VerifyFull)
            /* TODO: supply ssl client cert */
         */
        ;
    let pg_options = match cfg.database.auth {
        DatabaseAuth::PasswordAuth(PasswordAuth {
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
        r#"insert into api_tokens values ($1, $2, $3, $4, null, $5, $6);"#,
        uuid,
        token.as_bytes(),
        user_id,
        inherit_user_perms,
        Utc::now(),
        Utc::now() + chrono::Duration::from_std(lifetime).unwrap(),
    )
    .execute(&pg_pool)
    .await
    .into_diagnostic()?;

    println!("token_id={uuid}");
    println!("token={token}");

    Ok(())
}
