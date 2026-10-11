//! `dora-migrate` — run-once database schema migrator.
//!
//! In the split deployment the DHCP/API services connect without migrating (see
//! [`ip_manager::postgres::PostgresDb::connect`]); this binary owns the schema.
//! It applies any pending migrations and exits, so it can run as a Kubernetes
//! `Job` (or an init step) gated before the services roll out. Idempotent —
//! re-running against an up-to-date database is a no-op.
use anyhow::{Context, Result};
use dora_core::{
    config::{
        cli::{self, Parser},
        trace,
    },
    tokio::runtime::Builder,
    tracing::*,
};
use ip_manager::postgres::PostgresDb;

fn main() -> Result<()> {
    // Load `.env` BEFORE clap parses: the `env` attributes read the process
    // environment at parse time, and dotenvy never overrides a variable that is
    // already set, so an explicit environment still wins (#64). The outcome is
    // logged once tracing is up.
    let dotenv = dotenv::dotenv();
    let config = cli::Config::parse();
    let trace_config = trace::Config::parse(&config.dora_log)?;
    debug!(?config, ?trace_config);
    match dotenv {
        Ok(path) => debug!(?path, ".env loaded"),
        Err(err) => debug!(?err, ".env file not loaded"),
    }

    // A single-threaded runtime is plenty: connect, migrate, drop.
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        // Postgres only: an embedded redb store migrates itself in-process.
        let database_url = config
            .database_url
            .parse::<ip_manager::url::StorageUrl>()
            .context("invalid DATABASE_URL / --database-url")?
            .into_postgres()?;
        info!(
            database_url = %ip_manager::url::redact_url(&database_url),
            "running migrations"
        );
        PostgresDb::migrate(&database_url)
            .await
            .context("failed to run migrations")?;
        info!("migrations up to date");
        Ok(())
    })
}
