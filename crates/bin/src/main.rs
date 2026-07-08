#![allow(clippy::cognitive_complexity)]
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use config::{
    DhcpConfig,
    reservations::{RuntimeReservation, RuntimeReservations},
};
use dora_core::{
    Register, Server,
    config::{
        cli::{self, Parser},
        trace,
    },
    dhcproto::{v4, v6},
    mode::{ServerMode, SharedMode},
    tokio::{self, runtime::Builder, signal, task::JoinHandle},
    tracing::*,
};
use external_api::{ExternalApi, Health};
use ip_manager::{IpManager, sqlite::SqliteDb};
use leases::Leases;
use leases_v6::LeasesV6;
use message_type::MsgType;
use static_addr::StaticAddr;

#[cfg(not(target_env = "musl"))]
use jemallocator::Jemalloc;
use tokio_util::sync::CancellationToken;

#[cfg(not(target_env = "musl"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<()> {
    // parses from cli or environment var
    let config = cli::Config::parse();
    let trace_config = trace::Config::parse(&config.dora_log)?;
    debug!(?config, ?trace_config);
    if let Err(err) = dotenv::dotenv() {
        debug!(?err, ".env file not loaded");
    }

    let mut builder = Builder::new_multi_thread();
    // configure thread name & enable IO/time
    builder.thread_name(&config.thread_name).enable_all();
    // default num threads will be num logical CPUs
    // if we have a configured value here, set it
    if let Some(num) = config.threads {
        builder.worker_threads(num);
    }
    // build the runtime
    let rt = builder.build()?;

    rt.block_on(async move {
        match dora_core::tokio::spawn(async move { start(config).await }).await {
            Err(err) => error!(?err, "failed to start server"),
            Ok(Err(err)) => error!(?err, "exited with error"),
            Ok(_) => debug!("exiting..."),
        }
    });

    Ok(())
}

async fn start(config: cli::Config) -> Result<()> {
    let database_url = config.database_url.clone();
    info!(?database_url, "using database at path");
    let dora_id = config.dora_id.clone();
    info!(?dora_id, "using id");
    // setting DORA_ID for other plugins
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("DORA_ID", &dora_id) };

    debug!("parsing DHCP config");
    let dhcp_cfg = Arc::new(DhcpConfig::parse(&config.config_path)?);
    debug!("starting database");
    let ip_mgr = Arc::new(IpManager::new(SqliteDb::new(database_url).await?)?);
    // shared server mode: the management API sets it (maintenance / drain /
    // shutdown) and the DHCP datapath reads it to decide whether to answer.
    let mode = SharedMode::new(ServerMode::Normal);
    // shared runtime (API-managed) reservations: the management API mutates them
    // and the DHCP datapath reads them, overriding config reservations and the
    // pool. Warm the in-memory store from the database.
    let reservations = RuntimeReservations::new();
    match ip_mgr.list_reservations().await {
        Ok(records) => {
            let loaded: Vec<_> = records
                .iter()
                .filter_map(|r| {
                    match RuntimeReservation::from_parts(
                        &r.family,
                        &r.ip,
                        r.prefix.as_deref(),
                        r.network.clone(),
                        &r.match_json,
                    ) {
                        Ok(res) => Some(res),
                        Err(err) => {
                            warn!(?err, ip = %r.ip, "skipping malformed runtime reservation");
                            None
                        }
                    }
                })
                .collect();
            info!(count = loaded.len(), "loaded runtime reservations");
            reservations.load(loaded);
        }
        Err(err) => error!(?err, "failed to load runtime reservations from database"),
    }
    // start external api for healthchecks
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
    // start v4 server
    debug!("starting v4 server");
    let mut v4: Server<v4::Message> =
        Server::new(config.clone(), dhcp_cfg.v4().interfaces().to_owned())?;
    debug!("starting v4 plugins");

    // perhaps with only one plugin chain we will just register deps here
    // in order? we could get rid of derive macros & topo sort
    MsgType::new(Arc::clone(&dhcp_cfg))?
        .with_mode(mode.clone())
        .register(&mut v4);
    StaticAddr::new(Arc::clone(&dhcp_cfg))?
        .with_reservations(reservations.clone())
        .register(&mut v4);
    // leases plugin

    Leases::new(Arc::clone(&dhcp_cfg), Arc::clone(&ip_mgr)).register(&mut v4);

    let v6 = if dhcp_cfg.has_v6() {
        // start v6 server
        info!("starting v6 server");
        let mut v6: Server<v6::Message> =
            Server::new(config.clone(), dhcp_cfg.v6().interfaces().to_owned())?;
        info!("starting v6 plugins");
        MsgType::new(Arc::clone(&dhcp_cfg))?
            .with_mode(mode.clone())
            .register(&mut v6);
        LeasesV6::new(Arc::clone(&dhcp_cfg), Arc::clone(&ip_mgr))
            .with_reservations(reservations.clone())
            .register(&mut v6);
        Some(v6)
    } else {
        None
    };

    debug!("changing health to good");
    api.sender()
        .send(Health::Good)
        .await
        .context("error occurred in changing health status to Good")?;

    let token = CancellationToken::new();
    // if dropped, will stop server
    let api_guard = api.start(token.clone());
    match v6 {
        Some(v6) => {
            tokio::try_join!(
                flatten(tokio::spawn(v4.start(shutdown_signal(token.clone())))),
                flatten(tokio::spawn(v6.start(shutdown_signal(token.clone())))),
            )?;
        }
        None => {
            tokio::spawn(v4.start(shutdown_signal(token.clone()))).await??;
        }
    };
    if let Err(err) = api_guard.await {
        error!(?err, "error waiting for web server API");
    }
    Ok(())
}

async fn flatten<T>(handle: JoinHandle<Result<T, anyhow::Error>>) -> Result<T, anyhow::Error> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(anyhow!(err)),
    }
}

async fn shutdown_signal(token: CancellationToken) -> Result<()> {
    // Resolve on either Ctrl-C or a cancellation of the shared token (e.g. the
    // management API's `shutdown` action). Without the token branch, an
    // API-triggered shutdown would stop the HTTP API but leave the DHCP servers
    // running, since they wait on this future rather than the token directly.
    tokio::select! {
        ret = signal::ctrl_c() => {
            // propagate the Ctrl-C to the rest of the system (API + other server)
            token.cancel();
            ret.map_err(|err| anyhow!(err))
        }
        _ = token.cancelled() => Ok(()),
    }
}
