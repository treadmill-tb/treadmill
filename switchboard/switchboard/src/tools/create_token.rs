//! Backend for the `swx create-token` command.
//!
//! Creates an API token owned by a specified user with a specified lifetime.

use crate::server::database_from_config;
use crate::server::token::SecurityToken;
use chrono::Utc;
use miette::IntoDiagnostic;
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
    let cfg = crate::cfg::load_config(&cfg)?;
    let pg_pool = database_from_config(&cfg).await?;

    let uuid = Uuid::new_v4();
    let token = SecurityToken::generate();

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
