use crate::cfg::{Config, DatabaseAuth, PasswordAuth};
use miette::{IntoDiagnostic, WrapErr};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use tracing::instrument;

pub struct State {
    db_pool: PgPool,
}

#[instrument]
pub async fn serve(config_path: &Path) -> miette::Result<()> {
    tracing_subscriber::fmt().init();

    let cfg_text = std::fs::read_to_string(config_path)
        .into_diagnostic()
        .wrap_err("Failed to open configuration file")?;
    let cfg: Config = deser_hjson::from_str(&cfg_text)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "Failed to parse configuration file {}",
                config_path.display()
            )
        })?;

    tracing::info!("Serving with configuration: {cfg:?}");

    let pg_options = PgConnectOptions::new()
        .host(&cfg.database.address)
        .port(cfg.database.port);
    let pg_options = match cfg.database.auth {
        DatabaseAuth::PasswordAuth(PasswordAuth { username, password }) => {
            pg_options.username(&username).password(&password)
        }
    };
    let pg_pool = PgPool::connect_with(pg_options)
        .await
        .into_diagnostic()
        .wrap_err("Failed to connect to database")?;

    tracing::info!("Connected to database, poolsz_idle={}", pg_pool.num_idle());

    let r = sqlx::query!("select * from \"users\";")
        .fetch_all(&pg_pool)
        .await
        .unwrap();
    dbg!(r);

    Ok(())
}
