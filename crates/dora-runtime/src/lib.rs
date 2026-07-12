//! Shared bootstrap for the split dora services.
//!
//! `dora` used to be a single process that ran the v4 server, the v6 server, and
//! the management API together. It is now three separate binaries (`dora-v4`,
//! `dora-v6`, `dora-api`) plus a `dora-migrate` job. Everything those binaries have in
//! common — building the tokio runtime, connecting to Postgres, warm-loading the
//! runtime reservations, and the shutdown plumbing — lives here so each service's
//! `main` only has to wire its own role on top.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use config::{
    DhcpConfig,
    reservations::{RuntimeReservation, RuntimeReservations},
};
use dora_core::{
    config::cli,
    mode::{ServerMode, SharedMode},
    tokio::{self, runtime::Builder, signal, task::JoinHandle},
    tracing::*,
};
use ip_manager::{IpManager, postgres::PostgresDb};
use tokio_util::sync::CancellationToken;

/// State shared by every service: the parsed DHCP config, the IP manager backed
/// by the shared Postgres store, the API-managed server mode, the API-managed
/// runtime reservations, and the process-wide cancellation token.
///
/// In the split deployment `mode` and `reservations` are per-process — the API
/// process mutating them no longer reaches the v4/v6 datapaths in memory; the
/// datapaths coordinate through the database (reservations are DB-backed and
/// warm-loaded below). Each service still constructs both so the shared plugin
/// and API code keeps its existing shape.
pub struct Shared {
    pub dhcp_cfg: Arc<DhcpConfig>,
    pub ip_mgr: Arc<IpManager<PostgresDb>>,
    pub mode: SharedMode,
    pub reservations: RuntimeReservations,
    pub token: CancellationToken,
}

/// Build the multi-threaded tokio runtime the way every service wants it: named
/// worker threads, all IO/time drivers enabled, and the configured worker-thread
/// count (defaulting to the number of logical CPUs when unset).
pub fn build_runtime(config: &cli::Config) -> Result<tokio::runtime::Runtime> {
    let mut builder = Builder::new_multi_thread();
    builder.thread_name(&config.thread_name).enable_all();
    if let Some(num) = config.threads {
        builder.worker_threads(num);
    }
    Ok(builder.build()?)
}

/// The role-agnostic startup shared by every service: publish `DORA_ID` for the
/// plugins, parse the DHCP config, connect to Postgres (assuming `dora-migrate`
/// already applied the schema — see [`PostgresDb::connect`]), and warm-load the
/// runtime reservations from the database into the in-memory store.
pub async fn bootstrap(config: &cli::Config) -> Result<Shared> {
    let database_url = config.database_url.clone();
    info!(?database_url, "connecting to database");
    let dora_id = config.dora_id.clone();
    info!(?dora_id, "using id");
    // setting DORA_ID for other plugins
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("DORA_ID", &dora_id) };

    debug!("parsing DHCP config");
    let dhcp_cfg = Arc::new(DhcpConfig::parse(&config.config_path)?);
    debug!("connecting to database");
    // Schema is owned by the run-once `dora-migrate` job; services connect without
    // migrating so multiple services/replicas never race on migrations.
    let ip_mgr = Arc::new(IpManager::new(PostgresDb::connect(database_url).await?)?);
    // shared server mode: the management API sets it (maintenance / drain /
    // shutdown) and the DHCP datapath reads it to decide whether to answer. In
    // the split deployment the API is a separate process, so the mode is durably
    // stored in the database; warm the in-memory handle from there on startup and
    // keep it converged via `spawn_state_refresher`.
    let mode = SharedMode::new(ServerMode::Normal);
    match ip_mgr.get_server_mode().await {
        Ok(Some(s)) => {
            let m = ServerMode::from_db_str(&s);
            info!(?m, "loaded server mode from database");
            mode.set(m);
        }
        Ok(None) => {}
        Err(err) => warn!(?err, "failed to load server mode; defaulting to normal"),
    }
    // shared runtime (API-managed) reservations: the management API mutates them
    // (and persists them) and the DHCP datapath reads them, overriding config
    // reservations and the pool. Warm the in-memory store from the database.
    let reservations = RuntimeReservations::new();
    load_reservations(&ip_mgr, &reservations).await;

    Ok(Shared {
        dhcp_cfg,
        ip_mgr,
        mode,
        reservations,
        token: CancellationToken::new(),
    })
}

/// How often the DHCP servers re-read the API-managed state (mode + runtime
/// reservations) from the database into their in-memory handles.
const STATE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Reload the runtime reservations from the database into the in-memory store,
/// replacing its contents. Best-effort: a database error is logged and the
/// previous contents are kept.
async fn load_reservations(ip_mgr: &IpManager<PostgresDb>, reservations: &RuntimeReservations) {
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
                        r.options_json.as_deref(),
                        r.class.clone(),
                        r.lease_time.map(|l| l as u32),
                    ) {
                        Ok(res) => Some(res),
                        Err(err) => {
                            warn!(?err, ip = %r.ip, "skipping malformed runtime reservation");
                            None
                        }
                    }
                })
                .collect();
            debug!(count = loaded.len(), "loaded runtime reservations");
            reservations.load(loaded);
        }
        Err(err) => error!(?err, "failed to load runtime reservations from database"),
    }
}

/// Spawn a background task that keeps a DHCP server's in-memory state converged
/// with the database, so changes made through the (separate-process) management
/// API take effect without a restart. Every [`STATE_REFRESH_INTERVAL`] it
/// re-reads the server mode and the runtime reservations. Stops when the shared
/// token is cancelled.
pub fn spawn_state_refresher(shared: &Shared) -> JoinHandle<()> {
    let ip_mgr = Arc::clone(&shared.ip_mgr);
    let mode = shared.mode.clone();
    let reservations = shared.reservations.clone();
    let token = shared.token.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(STATE_REFRESH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = ticker.tick() => {
                    match ip_mgr.get_server_mode().await {
                        Ok(Some(s)) => mode.set(ServerMode::from_db_str(&s)),
                        Ok(None) => {}
                        Err(err) => warn!(?err, "server mode refresh failed"),
                    }
                    load_reservations(&ip_mgr, &reservations).await;
                }
            }
        }
    })
}

/// Resolve on either Ctrl-C or a cancellation of the shared token (e.g. another
/// service's shutdown). A DHCP server task waits on this rather than the token
/// directly so a local Ctrl-C also propagates out to the rest of the system.
pub async fn shutdown_signal(token: CancellationToken) -> Result<()> {
    tokio::select! {
        ret = signal::ctrl_c() => {
            // propagate the Ctrl-C to the rest of the system
            token.cancel();
            ret.map_err(|err| anyhow!(err))
        }
        _ = token.cancelled() => Ok(()),
    }
}

/// Flatten a `JoinHandle<Result<T>>` into a `Result<T>`, turning a join error
/// (panic / cancellation) into an `anyhow` error.
pub async fn flatten<T>(handle: JoinHandle<Result<T, anyhow::Error>>) -> Result<T, anyhow::Error> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(anyhow!(err)),
    }
}
