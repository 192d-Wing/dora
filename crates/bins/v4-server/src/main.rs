//! `dora-v4` — the DHCPv4 server service.
//!
//! One of the three services `dora` split into. It owns the v4 datapath only:
//! the shared bootstrap (config, database, reservations) comes from
//! [`dora_runtime`], and this binary registers the v4 plugin chain and runs the
//! `Server<v4::Message>`.
use std::sync::Arc;

use anyhow::Result;
use dora_core::{
    Register, Server,
    config::{
        cli::{self, Parser},
        trace,
    },
    dhcproto::v4,
    tokio,
    tracing::*,
};
use dora_runtime::{
    Shared, bootstrap, build_runtime, flatten, shutdown_signal, spawn_state_refresher,
};
use leases::Leases;
use message_type::MsgType;
use static_addr::StaticAddr;

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
    let shared = bootstrap(&config).await?;
    // keep this process's in-memory mode + reservations converged with changes
    // the (separate-process) management API writes to the database.
    spawn_state_refresher(&shared);
    let Shared {
        dhcp_cfg,
        ip_mgr,
        mode,
        reservations,
        token,
    } = shared;

    info!("starting v4 server");
    let mut v4: Server<v4::Message> =
        Server::new(config.clone(), dhcp_cfg.v4().interfaces().to_owned())?;
    debug!("starting v4 plugins");
    MsgType::new(Arc::clone(&dhcp_cfg))?
        .with_mode(mode.clone())
        .register(&mut v4);
    StaticAddr::new(Arc::clone(&dhcp_cfg))?
        .with_reservations(reservations.clone())
        .register(&mut v4);
    Leases::new(Arc::clone(&dhcp_cfg), Arc::clone(&ip_mgr)).register(&mut v4);

    flatten(tokio::spawn(v4.start(shutdown_signal(token.clone())))).await
}
