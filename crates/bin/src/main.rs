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
use ip_manager::{IpManager, postgres::PostgresDb};
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
    info!(?database_url, "connecting to database");
    let dora_id = config.dora_id.clone();
    info!(?dora_id, "using id");
    // setting DORA_ID for other plugins
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("DORA_ID", &dora_id) };

    debug!("parsing DHCP config");
    let dhcp_cfg = Arc::new(DhcpConfig::parse(&config.config_path)?);
    debug!("starting database");
    let ip_mgr = Arc::new(IpManager::new(PostgresDb::new(database_url).await?)?);
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
    // A dora process runs one or more roles: the v4 server, the v6 server, and
    // the management API. By default it runs all three in one process; `--role`
    // narrows that so each container in a split deployment runs just its part
    // against the shared database.
    let roles = config.active_roles();
    info!(?roles, "active roles");

    // shared cancellation token: the API's shutdown action and Ctrl-C both
    // cancel it, which stops every started component.
    let token = CancellationToken::new();

    // management API (role: api)
    let api_guard = if config.runs_api() {
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
        // if dropped, will stop the API
        Some(api.start(token.clone()))
    } else {
        None
    };

    // DHCP server tasks, one per active server role
    let mut servers: Vec<JoinHandle<Result<()>>> = Vec::new();

    // v4 server (role: v4)
    if config.runs_v4() {
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
        servers.push(tokio::spawn(v4.start(shutdown_signal(token.clone()))));
    }

    // v6 server (role: v6), only when the config actually has a v6 section
    if config.runs_v6() {
        if dhcp_cfg.has_v6() {
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
            servers.push(tokio::spawn(v6.start(shutdown_signal(token.clone()))));
        } else if !config.roles.is_empty() {
            // v6 was explicitly requested but the config has no v6 section
            warn!("--role v6 requested but the config has no v6 section; not starting v6");
        }
    }

    if servers.is_empty() && api_guard.is_none() {
        return Err(anyhow!("no active roles: nothing to run"));
    }

    // When no DHCP server is running (API-only), nothing else is listening for
    // Ctrl-C, so wire it to the shared token here. With servers present each one
    // watches Ctrl-C via `shutdown_signal`.
    if servers.is_empty() {
        let token = token.clone();
        tokio::spawn(async move {
            if signal::ctrl_c().await.is_ok() {
                token.cancel();
            }
        });
    }

    // wait for every DHCP server to exit (they stop together on token cancel /
    // Ctrl-C); propagate the first error
    for handle in servers {
        flatten(handle).await?;
    }
    if let Some(guard) = api_guard
        && let Err(err) = guard.await
    {
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
