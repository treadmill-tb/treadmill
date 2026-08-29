//! Database migrations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use sqlx::PgPool;
use sqlx::migrate::{Migrate, MigrateError, Migrator};
use sqlx::pool::PoolConnection;

use crate::serve::pg_pool_from_config;

/// Apply pending migrations, then exit.
#[derive(clap::Args, Debug)]
pub struct MigrateCommand {
    #[arg(short = 'c', long = "config", env = "TML_CFG_FILE")]
    config: Option<PathBuf>,

    /// Report what would be applied without touching the database.
    ///
    /// This does not attempt to perform migration, and does not surface errors
    /// that could happen when doing so.
    #[arg(long)]
    dry_run: bool,
}

/// [`Migrator`] that captures migrations embedded in this binary.
pub fn migrator() -> Migrator {
    sqlx::migrate!()
}

pub async fn migrate(cmd: MigrateCommand) -> anyhow::Result<()> {
    let config = crate::config::load_configuration(cmd.config.as_deref())?;
    let _sentry = crate::observability::init(config.sentry.as_ref())?;

    let pg_pool = pg_pool_from_config(&config.database)
        .await
        .context("failed to connect to database")?;

    let result = if cmd.dry_run {
        report_pending(&pg_pool).await
    } else {
        apply(&pg_pool).await
    };

    result.inspect_err(|e| {
        sentry::integrations::anyhow::capture_anyhow(e);
    })
}

/// Bring the database up to the embedded set, applying whatever is missing.
pub async fn apply(pg_pool: &PgPool) -> anyhow::Result<()> {
    migrator()
        .run(pg_pool)
        .await
        .context("failed to migrate database")?;

    Ok(())
}

/// Check that the database has exactly the migrations this binary embeds,
/// without applying anything.
pub async fn verify(pg_pool: &PgPool) -> anyhow::Result<()> {
    let migrator = migrator();
    let mut conn = pg_pool
        .acquire()
        .await
        .context("failed to acquire a connection to verify the schema")?;

    let applied = applied_migrations(&mut conn, &migrator).await?;

    // A migration recorded as started but not finished. Only reachable for `--
    // no-transaction` migrations.
    if applied.is_some()
        && let Some(version) = conn
            .dirty_version(&migrator.table_name)
            .await
            .context("failed to read the migration bookkeeping table")?
    {
        bail!(
            "database is dirty: migration {version} was interrupted partway through and \
             left the schema in a state no migration describes. It cannot be repaired by \
             re-running migrations; restore the pre-deployment snapshot instead."
        );
    }

    let applied = applied.unwrap_or_default();

    let mut problems = Vec::new();

    for embedded in migrator.iter() {
        match applied.get(&embedded.version) {
            None => problems.push(format!(
                "migration {} ({}) has not been applied",
                embedded.version, embedded.description
            )),
            Some(checksum) if checksum[..] != embedded.checksum[..] => problems.push(format!(
                "migration {} ({}) was applied from a different file than the one embedded \
                 in this binary",
                embedded.version, embedded.description
            )),
            Some(_) => {}
        }
    }

    // BTreeMap, so this is ordered by version rather than by hash iteration.
    for version in applied.keys() {
        if !migrator.version_exists(*version) {
            problems.push(format!(
                "migration {version} is applied to this database but is not embedded in this \
                 binary, which is older than the schema it was pointed at"
            ));
        }
    }

    if !problems.is_empty() {
        bail!(
            "database schema does not match this binary:\n{}",
            problems
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    Ok(())
}

/// List migrations that [`apply`] would run, without running them.
async fn report_pending(pg_pool: &PgPool) -> anyhow::Result<()> {
    let migrator = migrator();
    let mut conn = pg_pool
        .acquire()
        .await
        .context("failed to acquire a connection to inspect the schema")?;

    let applied = applied_migrations(&mut conn, &migrator)
        .await?
        .unwrap_or_default();

    let pending: Vec<_> = migrator
        .iter()
        .filter(|m| !applied.contains_key(&m.version))
        .collect();

    if pending.is_empty() {
        println!("No pending migrations; the database is up to date.");
    } else {
        println!("{} migration(s) would be applied:", pending.len());
        for m in pending {
            println!("  {} {}", m.version, m.description);
        }
    }

    Ok(())
}

/// Postgres `undefined_table`.
const UNDEFINED_TABLE: &str = "42P01";

/// The migrations recorded against this database, or `None` if the bookkeeping
/// table does not exist yet.
async fn applied_migrations(
    conn: &mut PoolConnection<sqlx::Postgres>,
    migrator: &Migrator,
) -> anyhow::Result<Option<BTreeMap<i64, Vec<u8>>>> {
    match conn.list_applied_migrations(&migrator.table_name).await {
        Ok(applied) => Ok(Some(
            applied
                .into_iter()
                .map(|m| (m.version, m.checksum.into_owned()))
                .collect(),
        )),
        Err(MigrateError::Execute(sqlx::Error::Database(e)))
            if e.code().as_deref() == Some(UNDEFINED_TABLE) =>
        {
            Ok(None)
        }
        Err(e) => Err(e).context("failed to read the migration bookkeeping table"),
    }
}
