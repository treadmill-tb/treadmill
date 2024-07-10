use crate::cfg::{Config, DatabaseAuth, PasswordAuth};
use crate::server::token::ApiToken;
use chrono::Utc;
use miette::{Context, IntoDiagnostic};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

pub async fn create_token(config_path: &Path, created_by_user_id: Uuid) -> miette::Result<()> {
    let cfg_text = std::fs::read_to_string(config_path)
        .into_diagnostic()
        .wrap_err("Failed to open configuration file")?;
    let cfg: Config = toml::from_str(&cfg_text)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "Failed to parse configuration file {}",
                config_path.display()
            )
        })?;
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
        r#"insert into api_tokens values ($1, $2, $3, null, $4, $5, $6, $7);"#,
        uuid,
        token.as_bytes(),
        created_by_user_id,
        Utc::now(),
        Utc::now(),
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
