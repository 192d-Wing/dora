//! `dora-v6` — the DHCPv6 server service.
//!
//! Owns the v6 datapath only. If the shared config has no v6 section there is
//! nothing for this service to do, so it logs a warning and exits cleanly — that
//! way the same config can be handed to all three services in a split deployment
//! without the v6 service crashing when v6 is unconfigured.
use std::sync::Arc;

use anyhow::Result;
use dora_core::{
    Register, Server,
    config::{
        cli::{self, Parser},
        trace,
    },
    dhcproto::v6,
    tokio,
    tracing::*,
};
use dora_runtime::{Shared, bootstrap, build_runtime, flatten, shutdown_signal};
use leases_v6::LeasesV6;
use message_type::MsgType;

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

    if !dhcp_cfg.has_v6() {
        warn!("config has no v6 section; nothing for the v6 server to do, exiting");
        return Ok(());
    }

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

    flatten(tokio::spawn(v6.start(shutdown_signal(token.clone())))).await
}
