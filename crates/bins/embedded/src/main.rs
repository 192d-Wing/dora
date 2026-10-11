//! `dora-embedded` — every dora role in one process on an embedded store.
//!
//! Runs the DHCPv4 server, the DHCPv6 server (when the config has a v6
//! section) and the management API together, backed by a single
//! [redb](https://github.com/cberner/redb) file instead of PostgreSQL. Meant
//! for routers and other single hosts: no database server, no `dora-migrate`
//! step (the store upgrades its own schema on open), and no state refresher —
//! the API mutates the same in-memory mode / reservation handles the datapath
//! reads, so its changes apply immediately.
//!
//! redb allows one writer process per file, so this binary is the whole
//! deployment: run the split `dora-v4` / `dora-v6` / `dora-api` services with
//! PostgreSQL when roles need to scale independently.
use std::sync::Arc;

use anyhow::{Context, Result};
use dora_core::{
    Register, Server,
    config::{
        cli::{self, CommandFactory, FromArgMatches, Parser},
        trace,
    },
    dhcproto::{v4, v6},
    tokio::{self, task::JoinHandle},
    tracing::*,
};
use dora_runtime::{Shared, bootstrap_with, build_runtime, flatten, shutdown_signal};
use external_api::{ExternalApi, Health};
use ip_manager::{
    redb::{RedbDb, RedbOptions, SyncMode},
    url::StorageUrl,
};
use leases::Leases;
use leases_v6::LeasesV6;
use message_type::MsgType;
use static_addr::StaticAddr;

#[cfg(not(target_env = "musl"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "musl"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// The shared dora CLI plus the embedded store's own knobs.
#[derive(Parser, Debug, Clone)]
#[clap(name = "dora-embedded", bin_name = "dora-embedded", about, long_about = None)]
struct EmbeddedCli {
    #[clap(flatten)]
    base: cli::Config,
    /// How lease writes reach the disk: `immediate` fsyncs every commit (a
    /// lease is on disk before the ACK goes out), `lazy` flushes periodically
    /// (fewer flash writes; a crash can forget the last second of leases).
    #[clap(long, env = "DORA_REDB_SYNC", default_value = "immediate")]
    redb_sync: SyncMode,
    /// Flush period, in milliseconds, for `--redb-sync lazy`.
    #[clap(long, env = "DORA_REDB_FLUSH_MS", default_value_t = 1000)]
    redb_flush_ms: u64,
}

impl EmbeddedCli {
    /// Parse like `cli::Config::parse()`, but with the embedded store as the
    /// default `DATABASE_URL`.
    fn parse_embedded() -> Self {
        let cmd = Self::command().mut_arg("database_url", |a| {
            a.default_value(cli::DEFAULT_EMBEDDED_DATABASE_URL)
        });
        let matches = cmd.get_matches();
        Self::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    }
}

fn main() -> Result<()> {
    // Load `.env` BEFORE clap parses: the `env` attributes read the process
    // environment at parse time, and dotenvy never overrides a variable that is
    // already set, so an explicit environment still wins (#64). The outcome is
    // logged once tracing is up.
    let dotenv = dotenv::dotenv();
    let cli = EmbeddedCli::parse_embedded();
    let trace_config = trace::Config::parse(&cli.base.dora_log)?;
    debug!(?cli, ?trace_config);
    match dotenv {
        Ok(path) => debug!(?path, ".env loaded"),
        Err(err) => debug!(?err, ".env file not loaded"),
    }

    let rt = build_runtime(&cli.base)?;
    rt.block_on(async move {
        match tokio::spawn(async move { start(cli).await }).await {
            Err(err) => error!(?err, "failed to start server"),
            Ok(Err(err)) => error!(?err, "exited with error"),
            Ok(_) => debug!("exiting..."),
        }
    });

    Ok(())
}

async fn start(cli: EmbeddedCli) -> Result<()> {
    let config = cli.base;

    // the store: a redb:// URL only
    let path = config
        .database_url
        .parse::<StorageUrl>()
        .context("invalid DATABASE_URL / --database-url")?
        .into_redb()?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating store directory {}", parent.display()))?;
    }
    let opts = RedbOptions {
        sync: cli.redb_sync,
        flush_interval: std::time::Duration::from_millis(cli.redb_flush_ms),
        ..RedbOptions::default()
    };
    info!(path = %path.display(), sync = ?opts.sync, "opening embedded store");
    // `open` is blocking (file lock + schema upgrade); keep it off the workers
    let store = {
        let open_path = path.clone();
        tokio::task::spawn_blocking(move || RedbDb::open(&open_path, opts))
            .await
            .context("store open task failed")?
            .with_context(|| format!("opening embedded store {}", path.display()))?
    };

    // One process: the API's mode / reservation writes land in the very handles
    // the datapath reads, so no `spawn_state_refresher` here.
    let Shared {
        dhcp_cfg,
        ip_mgr,
        mode,
        reservations,
        token,
    } = bootstrap_with(&config, store).await?;

    // management API
    let mut api = ExternalApi::new(
        config.external_api,
        Arc::clone(&dhcp_cfg),
        Arc::clone(&ip_mgr),
    )
    .with_mode(mode.clone())
    .with_reservations(reservations.clone());
    // enable in-process TLS (+ optional mTLS) when a cert/key pair is configured
    match (
        config.external_api_tls_cert.clone(),
        config.external_api_tls_key.clone(),
    ) {
        (Some(cert), Some(key)) => {
            api = api.with_tls(external_api::tls::TlsConfig {
                cert,
                key,
                client_ca: config.external_api_tls_client_ca.clone(),
                // recomputed inside with_tls based on whether a bearer token is set
                require_client_auth: false,
                reload_interval: std::time::Duration::from_secs(
                    config.external_api_tls_reload_secs,
                ),
            });
        }
        (None, None) => {
            if config.external_api_tls_client_ca.is_some() {
                warn!(
                    "--external-api-tls-client-ca set without --external-api-tls-cert/-key; \
                     mTLS is ignored and the API serves plaintext"
                );
            }
        }
        _ => warn!(
            "only one of --external-api-tls-cert / --external-api-tls-key set; serving plaintext"
        ),
    }
    debug!("changing health to good");
    api.sender()
        .send(Health::Good)
        .await
        .context("error occurred in changing health status to Good")?;
    // dropped (or token cancelled) => the API stops
    let api_guard = api.start(token.clone());

    let forensic = dhcp_cfg.forensic_log().is_some_and(|f| f.enabled);
    if forensic {
        info!("forensic logging enabled");
    }
    let mut servers: Vec<JoinHandle<Result<()>>> = Vec::new();

    // v4 server (always; it also owns the Ctrl-C -> token propagation via
    // `shutdown_signal`, which stops the API and v6 with it)
    info!("starting v4 server");
    let mut v4_srv: Server<v4::Message> =
        Server::new(config.clone(), dhcp_cfg.v4().interfaces().to_owned())?;
    debug!("starting v4 plugins");
    MsgType::new(Arc::clone(&dhcp_cfg))?
        .with_mode(mode.clone())
        .register(&mut v4_srv);
    StaticAddr::new(Arc::clone(&dhcp_cfg))?
        .with_reservations(reservations.clone())
        .register(&mut v4_srv);
    Leases::new(Arc::clone(&dhcp_cfg), Arc::clone(&ip_mgr)).register(&mut v4_srv);
    if forensic {
        v4_srv.postresponse(forensic_log::ForensicLog);
    }
    servers.push(tokio::spawn(v4_srv.start(shutdown_signal(token.clone()))));

    // v6 server, only when the config has a v6 section
    if dhcp_cfg.has_v6() {
        info!("starting v6 server");
        let mut v6_srv: Server<v6::Message> =
            Server::new(config.clone(), dhcp_cfg.v6().interfaces().to_owned())?;
        debug!("starting v6 plugins");
        MsgType::new(Arc::clone(&dhcp_cfg))?
            .with_mode(mode.clone())
            .register(&mut v6_srv);
        LeasesV6::new(Arc::clone(&dhcp_cfg), Arc::clone(&ip_mgr))
            .with_reservations(reservations.clone())
            .register(&mut v6_srv);
        if forensic {
            v6_srv.postresponse(forensic_log::ForensicLog);
        }
        servers.push(tokio::spawn(v6_srv.start(shutdown_signal(token.clone()))));
    } else {
        warn!("config has no v6 section; not starting the v6 server");
    }

    // the servers stop together on Ctrl-C / token cancel; propagate the first
    // error, then let the API wind down
    for handle in servers {
        flatten(handle).await?;
    }
    if let Err(err) = api_guard.await {
        error!(?err, "error waiting for web server API");
    }
    Ok(())
}
