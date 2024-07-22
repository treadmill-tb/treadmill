//! Backend for the `swx create-user` command.
//!
//! Creates a new user with the specified username, email, and password.

use argon2::password_hash::Salt;
use argon2::{Argon2, PasswordHasher};
use base64::prelude::BASE64_STANDARD_NO_PAD;
use base64::Engine;
use chrono::Utc;
use miette::{Context, IntoDiagnostic, Result, WrapErr};
use rand::RngCore;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::path::PathBuf;
use treadmill_rs::config::{self, TreadmillConfig};
use uuid::Uuid;

#[derive(Debug, clap::Parser)]
pub struct CreateUserCommand {
    #[clap(long, env = "TREADMILL_CONFIG")]
    config: Option<PathBuf>,

    /// Username of the user to create
    username: String,
    /// Email of the user to create
    email: String,
    /// Password to use for this user
    password: String,
}

pub async fn create_user(cmd: CreateUserCommand) -> Result<()> {
    let config = config::load_config()
        .into_diagnostic()
        .wrap_err("Failed to load configuration")?;

    let pg_options = PgConnectOptions::new()
        .host(&config.switchboard.database.address)
        .port(config.switchboard.database.port)
        .database(&config.switchboard.database.name);

    let pg_options = match config.switchboard.database.auth {
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
    let mut salt_bytes = [0u8; Salt::RECOMMENDED_LENGTH];
    rand::thread_rng().fill_bytes(&mut salt_bytes);
    let b64 = BASE64_STANDARD_NO_PAD.encode(&salt_bytes);
    let salt = Salt::from_b64(&b64).unwrap();
    let password_hash = Argon2::default()
        .hash_password(cmd.password.as_bytes(), salt)
        .unwrap()
        .to_string();

    sqlx::query!(
        r#"INSERT INTO users VALUES ($1, $2, $3, $4, $5, $6, $7);"#,
        uuid,
        cmd.username,
        cmd.email,
        password_hash,
        Utc::now(),
        Utc::now(),
        false
    )
    .execute(&pg_pool)
    .await
    .into_diagnostic()
    .wrap_err("Failed to insert new user into database")?;

    println!("user_id={uuid}");
    println!("name={}", cmd.username);
    println!("email={}", cmd.email);
    println!("password={}", cmd.password);

    Ok(())
}
