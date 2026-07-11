//! `dora-api` — the management / observability HTTP API service.
//!
//! Owns the external API only. It shares the database with the DHCP services and
//! exposes health, metrics, mode control, and runtime-reservation management. No
//! DHCP server runs here, so this binary wires Ctrl-C to the shared token itself
//! (the DHCP services do that via their `shutdown_signal`).
use std::sync::Arc;

use anyhow::{Context, Result};
use dora_core::{
    config::{
        cli::{self, Parser},
        trace,
    },
    tokio::{self, signal},
    tracing::*,
};
use dora_runtime::{Shared, bootstrap, build_runtime};
use external_api::{ExternalApi, Health};

#[cfg(not(target_env = "musl"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "musl"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<()> {
    let config = cli::Config::parse();
    let trace_config = trace::Config::parse(&config.dora_log)?;
    debug!(?config, ?trace_config);
    if let Err(err) = dotenv::dotenv() {
        debug!(?err, ".env file not loaded");
    }

    let rt = build_runtime(&config)?;
    rt.block_on(async move {
        match tokio::spawn(async move { start(config).await }).await {
            Err(err) => error!(?err, "failed to start server"),
            Ok(Err(err)) => error!(?err, "exited with error"),
            Ok(_) => debug!("exiting..."),
        }
    });

    Ok(())
}

async fn start(config: cli::Config) -> Result<()> {
    let Shared {
        dhcp_cfg,
        ip_mgr,
        mode,
        reservations,
        token,
    } = bootstrap(&config).await?;

    let mut api = ExternalApi::new(config.external_api, Arc::clone(&dhcp_cfg), Arc::clone(&ip_mgr))
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
                reload_interval: std::time::Duration::from_secs(config.external_api_tls_reload_secs),
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

    // No DHCP server is listening for Ctrl-C in this process, so wire it to the
    // shared token here; cancelling the token stops the API.
    {
        let token = token.clone();
        tokio::spawn(async move {
            if signal::ctrl_c().await.is_ok() {
                token.cancel();
            }
        });
    }

    let guard = api.start(token.clone());
    if let Err(err) = guard.await {
        error!(?err, "error waiting for web server API");
    }
    Ok(())
}
