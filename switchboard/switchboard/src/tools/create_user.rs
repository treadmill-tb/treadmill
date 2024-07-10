use crate::cfg::{Config, DatabaseAuth, PasswordAuth};
use argon2::password_hash::Salt;
use argon2::{Argon2, PasswordHasher};
use base64::prelude::BASE64_STANDARD_NO_PAD;
use base64::Engine;
use chrono::Utc;
use miette::{Context, IntoDiagnostic};
use rand::RngCore;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

pub async fn create_user(
    config_path: &Path,
    username: String,
    email: String,
    password: String,
) -> miette::Result<()> {
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
    let mut salt_bytes = [0u8; Salt::RECOMMENDED_LENGTH];
    rand::thread_rng().fill_bytes(&mut salt_bytes);
    let b64 = BASE64_STANDARD_NO_PAD.encode(&salt_bytes);
    let salt = Salt::from_b64(&b64).unwrap();
    let password_hash = Argon2::default()
        .hash_password(password.as_bytes(), salt)
        .unwrap()
        .to_string();

    sqlx::query!(
        r#"insert into users values ($1, $2, $3, $4, $5, $6, $7);"#,
        uuid,
        username,
        email,
        password_hash,
        Utc::now(),
        Utc::now(),
        false
    )
    .execute(&pg_pool)
    .await
    .into_diagnostic()?;

    println!("user_id={uuid}");
    println!("name={username}");
    println!("email={email}");
    println!("password={password}");

    Ok(())
}
