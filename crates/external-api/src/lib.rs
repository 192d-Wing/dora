//! # Management API
//!
//! This crate serves dora's JSON management/observability API: health and
//! readiness, server metadata, metrics, lease and reservation listings, and the
//! (redacted) running config. The full contract is `docs/openapi.yaml`, also
//! served at `GET /openapi.json` and rendered by a self-contained Swagger UI at
//! `GET /docs`. Public routes are `/health`, `/ready`, `/openapi.json`, and the
//! `/docs` assets; the rest are gated by a Bearer token when configured.
#![warn(
    missing_debug_implementations,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub,
    non_snake_case,
    non_upper_case_globals
)]
#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::cognitive_complexity, clippy::too_many_arguments)]

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use axum::{
    Router, extract::Extension, http::HeaderValue, middleware::map_response, response::Response,
    routing,
};

use config::reservations::RuntimeReservations;
use dora_core::mode::SharedMode;
use ip_manager::{IpManager, Storage};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace};

pub use crate::models::{Health, State};
pub mod tls;
use config::DhcpConfig;

/// The task runner for the [`ExternalApi`]
///
/// [`ExternalAPI`]: crate::ExternalApi
#[derive(Debug)]
pub struct ExternalApiGuard {
    task_handle: JoinHandle<()>,
}

impl Drop for ExternalApiGuard {
    fn drop(&mut self) {
        trace!("ExternalApiRunner drop called");
        self.task_handle.abort();
    }
}

/// Listens to relevant channels to gather information about
/// the running system and reports this data in an HTTP API
#[derive(Debug)]
pub struct ExternalApi<S> {
    tx: mpsc::Sender<Health>,
    rx: mpsc::Receiver<Health>,
    addr: SocketAddr,
    state: State,
    api_state: ApiState,
    auth: ApiAuth,
    ip_mgr: Arc<IpManager<S>>,
    cfg: Arc<DhcpConfig>,
    mode: SharedMode,
    reservations: RuntimeReservations,
    tls: Option<tls::TlsConfig>,
}

#[derive(Debug, Clone)]
struct ApiState {
    started_at: SystemTime,
}

#[derive(Debug, Clone)]
struct ApiAuth {
    bearer_token: Option<Arc<str>>,
    /// Explicit escape hatch for trusted local development. Production is
    /// fail-closed when neither a bearer token nor mTLS is configured.
    allow_unauthenticated: bool,
    /// whether mTLS client-cert auth is offered (a client-CA trust bundle is
    /// configured). Only affects what `auth_methods()` advertises; the actual
    /// per-request check reads the trusted `x-dora-client-verified` header the
    /// TLS layer stamps.
    mtls_enabled: bool,
}

impl ApiAuth {
    fn from_env() -> Self {
        let bearer_token = std::env::var("DORA_API_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .map(Arc::<str>::from);
        let allow_unauthenticated = std::env::var("DORA_API_ALLOW_UNAUTHENTICATED")
            .ok()
            .is_some_and(|value| {
                matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true")
            });

        Self {
            bearer_token,
            allow_unauthenticated,
            mtls_enabled: false,
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            bearer_token: None,
            allow_unauthenticated: true,
            mtls_enabled: false,
        }
    }

    #[cfg(test)]
    fn bearer(token: &str) -> Self {
        Self {
            bearer_token: Some(Arc::<str>::from(token)),
            allow_unauthenticated: false,
            mtls_enabled: false,
        }
    }

    fn auth_methods(&self) -> Vec<String> {
        let mut methods = Vec::new();
        if self.bearer_token.is_some() {
            methods.push("bearer".to_string());
        }
        if self.mtls_enabled {
            methods.push("mtls".to_string());
        }
        methods
    }
}

/// The header the TLS layer stamps (and always strips from client input first)
/// to mark a request as authenticated by a verified client certificate.
pub(crate) const MTLS_HEADER: &str = "x-dora-client-verified";

impl<S: Storage> ExternalApi<S> {
    /// Create a new ExternalApi instance
    pub fn new(addr: SocketAddr, cfg: Arc<DhcpConfig>, ip_mgr: Arc<IpManager<S>>) -> Self {
        trace!("starting external api");
        let (tx, rx) = mpsc::channel(10);
        let state = models::blank_health();
        Self {
            tx,
            rx,
            addr,
            state,
            api_state: ApiState {
                started_at: SystemTime::now(),
            },
            auth: ApiAuth::from_env(),
            ip_mgr,
            cfg,
            mode: SharedMode::default(),
            reservations: RuntimeReservations::new(),
            tls: None,
        }
    }

    /// Attach the shared runtime-reservation store so the create/update/delete
    /// reservation actions mutate the same reservations the datapath reads.
    pub fn with_reservations(mut self, reservations: RuntimeReservations) -> Self {
        self.reservations = reservations;
        self
    }

    /// Serve the API over TLS using the given cert/key (and optional client-cert
    /// trust anchors for mTLS), hot-reloading them on rotation. Without this the
    /// API serves plain HTTP (intended to sit behind a TLS-terminating proxy).
    pub fn with_tls(mut self, mut tls: tls::TlsConfig) -> Self {
        // advertise mtls in server metadata when a client-CA bundle is present
        self.auth.mtls_enabled = tls.client_ca.is_some();
        // If a client-CA is configured but there's no Bearer token, require a
        // valid client cert at the TLS layer — otherwise `authorize` (which is
        // open when no token is set) would let certless clients straight in,
        // making a `client_ca`-only deployment silently open.
        tls.require_client_auth = tls.client_ca.is_some() && self.auth.bearer_token.is_none();
        self.tls = Some(tls);
        self
    }

    /// Attach the shared server-mode handle so the `maintenance-mode`, `drain`,
    /// and `shutdown` actions drive the same mode the DHCP datapath reads, and
    /// `GET /v1/server` reports it. Without this the API reports `normal`.
    pub fn with_mode(mut self, mode: SharedMode) -> Self {
        self.mode = mode;
        self
    }

    /// clone the health sender channel
    pub fn sender(&self) -> mpsc::Sender<Health> {
        self.tx.clone()
    }

    /// Set the health
    pub async fn set_health(&self, health: Health) {
        *self.state.lock() = health;
    }

    /// Listen to Health changes over the channel
    async fn listen_status(&mut self) -> Result<()> {
        while let Some(health) = self.rx.recv().await {
            let mut guard = self.state.lock();
            if *guard != health {
                *guard = health;
            }
        }
        info!("listen health exited-- nothing listening");
        Ok(())
    }

    /// serve the HTTP external api
    async fn run(
        addr: SocketAddr,
        state: State,
        api_state: ApiState,
        auth: ApiAuth,
        cfg: Arc<DhcpConfig>,
        ip_mgr: Arc<IpManager<S>>,
        mode: SharedMode,
        reservations: RuntimeReservations,
        tls: Option<tls::TlsConfig>,
        token: CancellationToken,
    ) -> Result<()> {
        const TIMEOUT: u64 = 30;
        // the shutdown action cancels this same token to stop the server, so the
        // router gets a clone while the original drives graceful shutdown below
        let service = api_router::<S>(
            state,
            api_state,
            auth,
            cfg,
            ip_mgr,
            mode,
            reservations,
            token.clone(),
            Duration::from_secs(TIMEOUT),
        );

        let tcp = TcpListener::bind(&addr).await?;

        match tls {
            Some(tls_cfg) => {
                // terminate TLS in-process, hot-reloading certs in the background
                let state = tls::TlsState::load(tls_cfg)?;
                tokio::spawn(tls::reload_task(state.clone(), token.clone()));
                tracing::debug!(%addr, "external API listening (TLS)");
                tls::serve(tcp, state, service, token).await?;
            }
            None => {
                tracing::debug!(%addr, "external API listening (plaintext)");
                axum::serve(tcp, service)
                    .with_graceful_shutdown(async move {
                        token.cancelled().await;
                    })
                    .await?;
            }
        }
        Ok(())
    }

    /// Kick off the HTTP service and start listening on all channels for
    /// changes
    pub fn start(mut self, token: CancellationToken) -> JoinHandle<()> {
        let state = self.state.clone();
        let api_state = self.api_state.clone();
        let auth = self.auth.clone();
        let addr = self.addr;
        let ip_mgr = self.ip_mgr.clone();
        let cfg = self.cfg.clone();
        let mode = self.mode.clone();
        let reservations = self.reservations.clone();
        let tls = self.tls.clone();
        // if tx is not cloned, health listen will never update since ExternalApi is owner

        tokio::spawn(async move {
            // `run` will exit when cancel token completes
            tokio::select! {
                r = ExternalApi::run(addr, state, api_state, auth, cfg, ip_mgr, mode, reservations, tls, token) => {
                    if let Err(err) = r {
                        error!(?err, "external api task returned error")
                    }
                    // exiting
                }
                _ = self.listen_status() => {}
            }
        })
    }

    /// Start the `ExternalApiRunner`
    pub fn serve(self, token: CancellationToken) -> ExternalApiGuard {
        ExternalApiGuard {
            task_handle: self.start(token),
        }
    }
}

fn api_router<S: Storage>(
    state: State,
    api_state: ApiState,
    auth: ApiAuth,
    cfg: Arc<DhcpConfig>,
    ip_mgr: Arc<IpManager<S>>,
    mode: SharedMode,
    reservations: RuntimeReservations,
    shutdown: CancellationToken,
    timeout: Duration,
) -> Router {
    use axum::extract::DefaultBodyLimit;
    use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

    // Config candidates are the largest legitimate request. Keep a hard cap so
    // Bytes extractors cannot be used to force unbounded request buffering.
    const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

    Router::new()
        .route("/health", routing::get(handlers::health))
        .route("/ready", routing::get(handlers::ready))
        .route("/openapi.json", routing::get(handlers::openapi_json))
        .route("/docs", routing::get(handlers::docs_html))
        .route(
            "/docs/swagger-ui-bundle.js",
            routing::get(handlers::swagger_ui_bundle_js),
        )
        .route(
            "/docs/swagger-ui.css",
            routing::get(handlers::swagger_ui_css),
        )
        .route("/v1/server", routing::get(handlers::server_info))
        .route(
            "/v1/metrics/summary",
            routing::get(handlers::metrics_summary),
        )
        .route("/v1/metrics", routing::get(handlers::metrics_json))
        .route(
            "/v1/metrics/prometheus",
            routing::get(handlers::metrics_prometheus_json),
        )
        .route("/metrics", routing::get(handlers::metrics))
        .route("/metrics-text", routing::get(handlers::metrics_text))
        .route("/v1/leases", routing::get(handlers::leases::<S>))
        .route("/v1/leases/v4", routing::get(handlers::leases_v4::<S>))
        .route("/v1/leases/v6", routing::get(handlers::leases_v6::<S>))
        .route(
            "/v1/reservations/v4",
            routing::get(handlers::reservations_v4),
        )
        .route(
            "/v1/reservations/v6",
            routing::get(handlers::reservations_v6),
        )
        .route(
            "/v1/config",
            routing::get(handlers::config::<S>).put(handlers::create_config_candidate::<S>),
        )
        .route(
            "/v1/config/candidates",
            routing::get(handlers::list_config_candidates::<S>)
                .post(handlers::create_config_candidate::<S>),
        )
        .route(
            "/v1/config/candidates/{candidate_id}",
            routing::get(handlers::get_config_candidate::<S>),
        )
        .route(
            "/v1/operations/{operation_id}",
            routing::get(handlers::get_operation::<S>),
        )
        .route(
            "/v1/actions/reload",
            routing::post(handlers::reload_config::<S>),
        )
        .route(
            "/v1/actions/activate-config",
            routing::post(handlers::activate_config::<S>),
        )
        .route(
            "/v1/actions/rollback-config",
            routing::post(handlers::rollback_config::<S>),
        )
        .route(
            "/v1/actions/maintenance-mode",
            routing::post(handlers::maintenance_mode::<S>),
        )
        .route("/v1/actions/drain", routing::post(handlers::drain::<S>))
        .route(
            "/v1/actions/shutdown",
            routing::post(handlers::shutdown::<S>),
        )
        .route(
            "/v1/actions/create-reservation",
            routing::post(handlers::create_reservation::<S>),
        )
        .route(
            "/v1/actions/update-reservation",
            routing::post(handlers::update_reservation::<S>),
        )
        .route(
            "/v1/actions/delete-reservation",
            routing::post(handlers::delete_reservation::<S>),
        )
        .route(
            "/v1/actions/release-lease",
            routing::post(handlers::release_lease::<S>),
        )
        .route(
            "/v1/actions/trigger-ddns-update",
            routing::post(handlers::trigger_ddns::<S>),
        )
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(Extension(state))
        .layer(Extension(api_state))
        .layer(Extension(auth))
        .layer(Extension(ip_mgr))
        .layer(Extension(cfg))
        .layer(Extension(mode))
        .layer(Extension(reservations))
        .layer(Extension(shutdown))
        // stamp the trusted mTLS marker from the connection's verified client
        // cert (and strip any client-supplied spoof) before handlers run auth
        .layer(axum::middleware::from_fn(stamp_mtls))
        // outermost layer: guarantee every response carries an X-Request-ID
        // header — including the timeout responses generated by the TimeoutLayer
        // above. Handlers that set their own matching id are left untouched.
        .layer(map_response(ensure_request_id_header))
}

/// Replace the `x-dora-client-verified` header with a trusted value derived from
/// the TLS connection's verified client certificate. Any client-supplied value
/// is removed first, so a request can never forge mTLS authentication (including
/// over plaintext, where `ConnData` is absent and the header is simply cleared).
async fn stamp_mtls(mut request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    request.headers_mut().remove(MTLS_HEADER);
    let verified = request
        .extensions()
        .get::<tls::ConnData>()
        .is_some_and(|conn| conn.mtls_authenticated);
    if verified {
        request
            .headers_mut()
            .insert(MTLS_HEADER, HeaderValue::from_static("1"));
    }
    next.run(request).await
}

/// Backfill an `X-Request-ID` header on any response that doesn't already have
/// one, so every API response is traceable (RFC-less but standard practice).
async fn ensure_request_id_header(mut response: Response) -> Response {
    if !response.headers().contains_key("x-request-id")
        && let Ok(value) = HeaderValue::from_str(&handlers::request_id())
    {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

mod handlers {

    use std::{
        collections::{BTreeMap, HashMap, HashSet},
        net::IpAddr,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use anyhow::Context;
    use axum::{
        body::{Body, Bytes},
        extract::{Extension, Path, Query},
        http::header,
        http::{HeaderMap, HeaderValue, Response, StatusCode, header::AUTHORIZATION},
        response::IntoResponse,
    };
    use chrono::{DateTime, Utc};
    use config::DhcpConfig;
    use config::reservations::{RuntimeReservation, RuntimeReservations};
    use config::wire::v4::Condition;
    use dora_core::metrics::{START_TIME, UPTIME};
    use dora_core::mode::{ServerMode, SharedMode};
    use ip_manager::{
        ConfigCandidateRecord, IpManager, OperationRecord, OperationStatus,
        RuntimeReservationRecord, Storage,
    };
    use ipnet::Ipv4Net;
    use prometheus::{Encoder, ProtobufEncoder, TextEncoder};
    use serde::Deserialize;
    use serde::de::DeserializeOwned;
    use subtle::ConstantTimeEq;
    use tokio_util::sync::CancellationToken;
    use tracing::{error, warn};

    use crate::models::{
        ActionResult, ActivateConfigRequest, ConfigCandidate, ConfigCandidateListResponse,
        ConfigDocument, ConfigUpdateRequest, DeleteReservationRequest, DrainRequest, Health,
        HealthResponse, Histogram, HistogramBucket, MaintenanceModeRequest, MetricFamily,
        MetricSample, MetricsDetailed, MetricsSummary, OpenMetricsJson, Operation,
        OperationAccepted, OperationLinks, PaginationMeta, ProtocolMetricsSummary, ReadinessCheck,
        ReadinessResponse, ReadinessStatus, ReleaseLeaseRequest, ReloadRequest,
        ReservationMutationRequest, ReserveIp, RollbackConfigRequest, ServerApiInfo, ServerInfo,
        ServerResult, ShutdownRequest, State, TriggerDdnsRequest, V4Lease, V4LeaseListResponse,
        V4Reservation, V4ReservationListResponse, V6Lease, V6LeaseListResponse, V6Reservation,
        V6ReservationListResponse, ValidationMessage,
    };

    const OPENAPI_YAML: &str = include_str!("../../../docs/openapi.yaml");

    pub(crate) async fn health() -> ServerResult<impl IntoResponse> {
        let request_id = request_id();
        Ok((
            request_id_header(&request_id)?,
            axum::Json(HealthResponse {
                status: "alive".to_string(),
                request_id,
            }),
        ))
    }

    pub(crate) async fn ready(
        Extension(state): Extension<State>,
    ) -> ServerResult<impl IntoResponse> {
        let health = *state.lock();
        let ready = health == Health::Good;
        let request_id = request_id();
        let body = ReadinessResponse {
            status: if ready {
                ReadinessStatus::Ready
            } else {
                ReadinessStatus::NotReady
            },
            checks: vec![ReadinessCheck {
                name: "health".to_string(),
                status: if ready { "pass" } else { "fail" }.to_string(),
                message: None,
            }],
            request_id: request_id.clone(),
        };
        let status = if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };

        Ok((status, request_id_header(&request_id)?, axum::Json(body)))
    }

    pub(crate) async fn openapi_json() -> ServerResult<impl IntoResponse> {
        let request_id = request_id();
        let yaml: yaml_serde::Value =
            yaml_serde::from_str(OPENAPI_YAML).context("failed to parse embedded OpenAPI")?;
        Ok((
            request_id_header(&request_id)?,
            axum::Json(yaml_to_json(yaml)),
        ))
    }

    // Swagger UI is served self-contained (assets vendored into the binary) so
    // it works in the air-gapped hardened container with no CDN. Like
    // `openapi.json`, these are public — read-only docs, no data exposure.
    const SWAGGER_UI_HTML: &str = include_str!("../assets/swagger-ui/index.html");
    const SWAGGER_UI_JS: &str = include_str!("../assets/swagger-ui/swagger-ui-bundle.js");
    const SWAGGER_UI_CSS: &str = include_str!("../assets/swagger-ui/swagger-ui.css");

    pub(crate) async fn docs_html() -> impl IntoResponse {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            SWAGGER_UI_HTML,
        )
    }

    pub(crate) async fn swagger_ui_bundle_js() -> impl IntoResponse {
        (
            [
                (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
                // modest cache; not `immutable` since the URL is unversioned and
                // a re-vendor reuses the same filename (bounds staleness to 1h)
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            SWAGGER_UI_JS,
        )
    }

    pub(crate) async fn swagger_ui_css() -> impl IntoResponse {
        (
            [
                (header::CONTENT_TYPE, "text/css; charset=utf-8"),
                (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
            ],
            SWAGGER_UI_CSS,
        )
    }

    pub(crate) async fn metrics_summary(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        Ok((
            request_id_header(&request_id)?,
            axum::Json(build_metrics_summary(&prometheus::gather())),
        ))
    }

    pub(crate) async fn metrics_json(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let families = prometheus::gather();
        Ok((
            request_id_header(&request_id)?,
            axum::Json(build_metrics_detailed(&families)),
        ))
    }

    pub(crate) async fn metrics_prometheus_json(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        Ok((
            request_id_header(&request_id)?,
            axum::Json(OpenMetricsJson {
                families: prometheus::gather()
                    .into_iter()
                    .map(metric_family_to_json)
                    .collect(),
            }),
        ))
    }

    pub(crate) async fn server_info(
        headers: HeaderMap,
        Extension(api_state): Extension<crate::ApiState>,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(mode): Extension<SharedMode>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let started_at = DateTime::<Utc>::from(api_state.started_at).to_rfc3339();
        let auth_methods = auth.auth_methods();
        Ok((
            request_id_header(&request_id)?,
            axum::Json(ServerInfo {
                id: std::env::var("DORA_ID").unwrap_or_else(|_| "dora_id".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                started_at,
                mode: mode.get(),
                api: ServerApiInfo {
                    version: "v1".to_string(),
                    auth: auth_methods,
                },
                request_id,
            }),
        ))
    }

    // ---- operations & actions ---------------------------------------------

    /// `GET /v1/operations/{operation_id}` — report a persisted operation's
    /// lifecycle status.
    pub(crate) async fn get_operation<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Path(operation_id): Path<String>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let record = ip_mgr.get_operation(&operation_id).await?.ok_or_else(|| {
            crate::models::ServerError::not_found(format!("operation {operation_id} not found"))
        })?;
        Ok((
            request_id_header(&request_id)?,
            axum::Json(operation_to_json(record)),
        ))
    }

    /// `POST /v1/actions/maintenance-mode` — enter or leave maintenance mode
    /// (synchronous). In maintenance the datapath suppresses new leases *and*
    /// renewals.
    pub(crate) async fn maintenance_mode<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(mode): Extension<SharedMode>,
        body: Bytes,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        // shutting-down is terminal: refuse changes that would re-enable serving
        // while the graceful-shutdown grace period is counting down
        reject_if_shutting_down(&mode)?;
        let req: MaintenanceModeRequest = parse_required_body(&body)?;
        let target = if req.enabled {
            ServerMode::Maintenance
        } else {
            ServerMode::Normal
        };
        mode.set(target);
        // Persist so the (separate-process) DHCP servers converge to this mode;
        // in-memory `mode.set` only affects this API process.
        ip_mgr.set_server_mode(target.as_str()).await?;
        let result = serde_json::json!({ "mode": target });
        record_sync_action(
            &ip_mgr,
            "maintenance-mode",
            actor(&auth),
            serde_json::json!({ "enabled": req.enabled, "reason": req.reason }),
            result.clone(),
        )
        .await?;
        Ok((
            request_id_header(&request_id)?,
            axum::Json(ActionResult {
                status: "succeeded".to_string(),
                action: "maintenance-mode".to_string(),
                message: Some(format!(
                    "maintenance mode {}",
                    if req.enabled { "enabled" } else { "disabled" }
                )),
                result: Some(result),
            }),
        ))
    }

    /// `POST /v1/actions/drain` — enter drain mode (synchronous). New leases are
    /// suppressed; existing clients may still renew.
    pub(crate) async fn drain<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(mode): Extension<SharedMode>,
        body: Bytes,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        reject_if_shutting_down(&mode)?;
        let req: DrainRequest = parse_optional_body(&body)?;
        mode.set(ServerMode::Drain);
        // Persist so the (separate-process) DHCP servers converge to drain.
        ip_mgr.set_server_mode(ServerMode::Drain.as_str()).await?;
        let result = serde_json::json!({ "mode": ServerMode::Drain });
        record_sync_action(
            &ip_mgr,
            "drain",
            actor(&auth),
            serde_json::json!({ "reason": req.reason }),
            result.clone(),
        )
        .await?;
        Ok((
            request_id_header(&request_id)?,
            axum::Json(ActionResult {
                status: "succeeded".to_string(),
                action: "drain".to_string(),
                message: Some("server is draining; new leases suppressed".to_string()),
                result: Some(result),
            }),
        ))
    }

    /// `POST /v1/actions/shutdown` — begin a graceful shutdown (asynchronous,
    /// returns `202`). Enters shutting-down mode immediately and, after the grace
    /// period, cancels the shared token to stop the DHCP servers and this API.
    pub(crate) async fn shutdown<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(mode): Extension<SharedMode>,
        Extension(shutdown): Extension<CancellationToken>,
        body: Bytes,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        // a shutdown is already terminal; don't accept a second one
        reject_if_shutting_down(&mode)?;
        let req: ShutdownRequest = parse_optional_body(&body)?;
        // clamp the grace period so a mistaken huge value can't wedge the server
        // in shutting-down mode indefinitely (with no un-shutdown path)
        let grace_secs = req.grace_period_seconds.min(MAX_GRACE_PERIOD_SECONDS);
        let operation_id = new_operation_id();
        let now = SystemTime::now();
        let record = OperationRecord {
            operation_id: operation_id.clone(),
            action: "shutdown".to_string(),
            status: OperationStatus::Accepted,
            actor: actor(&auth),
            input_summary: Some(
                serde_json::json!({
                    "grace_period_seconds": grace_secs,
                    "reason": req.reason,
                })
                .to_string(),
            ),
            result: None,
            error_code: None,
            error_message: None,
            created_at: now,
            started_at: None,
            completed_at: None,
        };
        ip_mgr.insert_operation(&record).await?;
        // enter shutting-down mode now so the datapath drains new leases during
        // the grace period
        mode.set(ServerMode::ShuttingDown);
        // The DHCP datapaths are separate processes that read the mode from the
        // database. ShuttingDown is terminal and non-recoverable (there is no
        // un-shutdown path), so persisting it would wedge the datapaths — and a
        // restarted API — permanently. Persist Drain instead: it suppresses NEW
        // leases cluster-wide while existing clients keep renewing (the graceful
        // behavior), and it is recoverable via maintenance-mode/drain. This API
        // process keeps its local ShuttingDown for its own terminal guard/exit.
        ip_mgr.set_server_mode(ServerMode::Drain.as_str()).await?;

        // finish out of band: mark running, wait the grace period, mark succeeded,
        // then cancel the shared token (stops the DHCP servers and this API)
        let ip_mgr = Arc::clone(&ip_mgr);
        let grace = Duration::from_secs(grace_secs);
        tokio::spawn(async move {
            let mut record = record;
            record.status = OperationStatus::Running;
            record.started_at = Some(SystemTime::now());
            if let Err(err) = ip_mgr.update_operation(&record).await {
                error!(?err, "failed to mark shutdown operation running");
            }
            tokio::time::sleep(grace).await;
            record.status = OperationStatus::Succeeded;
            record.completed_at = Some(SystemTime::now());
            record.result = Some(serde_json::json!({ "shutdown": true }).to_string());
            if let Err(err) = ip_mgr.update_operation(&record).await {
                error!(?err, "failed to mark shutdown operation succeeded");
            }
            shutdown.cancel();
        });

        Ok((
            StatusCode::ACCEPTED,
            request_id_header(&request_id)?,
            axum::Json(OperationAccepted {
                operation_id: operation_id.clone(),
                status: "accepted".to_string(),
                links: Some(OperationLinks {
                    self_link: Some(format!("/v1/operations/{operation_id}")),
                }),
            }),
        ))
    }

    /// Upper bound on a shutdown grace period. Past this the server would sit in
    /// shutting-down mode with no way to recover, so cap it at one hour.
    const MAX_GRACE_PERIOD_SECONDS: u64 = 3600;

    /// Reject a mode-changing action once shutdown has begun. Shutting-down is
    /// terminal, so re-entering normal/drain/maintenance (or a second shutdown)
    /// is a `409 Conflict`.
    fn reject_if_shutting_down(mode: &SharedMode) -> ServerResult<()> {
        if mode.get() == ServerMode::ShuttingDown {
            Err(crate::models::ServerError::conflict(
                "server is shutting down",
            ))
        } else {
            Ok(())
        }
    }

    /// `POST /v1/actions/create-reservation` — add a runtime reservation.
    pub(crate) async fn create_reservation<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(reservations): Extension<RuntimeReservations>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize(&headers, &auth)?;
        let req: ReservationMutationRequest = parse_required_body(&body)?;
        write_reservation(
            &auth,
            &ip_mgr,
            &reservations,
            req,
            false,
            "create-reservation",
        )
        .await
    }

    /// `POST /v1/actions/update-reservation` — replace an existing runtime
    /// reservation (same address).
    pub(crate) async fn update_reservation<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(reservations): Extension<RuntimeReservations>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize(&headers, &auth)?;
        let req: ReservationMutationRequest = parse_required_body(&body)?;
        write_reservation(
            &auth,
            &ip_mgr,
            &reservations,
            req,
            true,
            "update-reservation",
        )
        .await
    }

    /// `POST /v1/actions/delete-reservation` — remove a runtime reservation by
    /// (family, ip).
    pub(crate) async fn delete_reservation<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(reservations): Extension<RuntimeReservations>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let req: DeleteReservationRequest = parse_required_body(&body)?;
        if req.family != "v4" && req.family != "v6" {
            return Err(crate::models::ServerError::bad_request(
                "family must be v4 or v6",
            ));
        }
        let ip: IpAddr = req.ip.parse().map_err(|_| {
            crate::models::ServerError::bad_request(format!("invalid ip {}", req.ip))
        })?;

        // persist first; only drop from the in-memory store if a row actually
        // existed, so a 404 leaves both DB and memory untouched
        let removed = ip_mgr.delete_reservation(&req.family, &req.ip).await?;
        if !removed {
            return Err(crate::models::ServerError::not_found(format!(
                "no {} reservation for {}",
                req.family, req.ip
            )));
        }
        reservations.remove(&req.family, ip);

        let result = serde_json::json!({ "family": req.family, "ip": req.ip });
        let operation_id = record_sync_action(
            &ip_mgr,
            "delete-reservation",
            actor(&auth),
            serde_json::json!({ "family": req.family, "ip": req.ip }),
            result.clone(),
        )
        .await?;
        reservation_response(
            &request_id,
            req.r#async,
            &operation_id,
            "delete-reservation",
            result,
        )
    }

    /// Shared create/update path: validate, apply to the in-memory store (which
    /// enforces conflicts), persist, record the audit operation, and respond.
    async fn write_reservation<S: Storage>(
        auth: &crate::ApiAuth,
        ip_mgr: &IpManager<S>,
        reservations: &RuntimeReservations,
        req: ReservationMutationRequest,
        replace: bool,
        action: &str,
    ) -> ServerResult<axum::response::Response> {
        let request_id = request_id();
        let async_flag = req.r#async;
        let res = parse_reservation(&req)?;

        // in-memory first so conflict detection happens before we persist;
        // keep the entry it replaced so we can fully roll back on a DB failure
        let replaced = reservations
            .insert(res.clone(), replace)
            .map_err(reservation_error_to_server)?;
        let record = RuntimeReservationRecord {
            family: res.family().to_string(),
            ip: res.ip_string(),
            prefix: res.prefix_string(),
            network: res.network.clone(),
            match_json: res.match_json(),
            created_at: SystemTime::now(),
        };
        if let Err(err) = ip_mgr.upsert_reservation(&record).await {
            // restore the prior state so the store matches the database: put back
            // the replaced entry (update), or drop the new one (create)
            match replaced {
                Some(old) => reservations.restore(old),
                None => {
                    reservations.remove(res.family(), res.ip);
                }
            }
            return Err(err.into());
        }

        let result = serde_json::json!({ "family": res.family(), "ip": res.ip_string() });
        let operation_id = record_sync_action(
            ip_mgr,
            action,
            actor(auth),
            serde_json::json!({ "family": res.family(), "ip": res.ip_string() }),
            result.clone(),
        )
        .await?;
        reservation_response(&request_id, async_flag, &operation_id, action, result)
    }

    /// Build a create/update/delete-reservation response: `202 OperationAccepted`
    /// when the caller asked for async, else `200 ActionResult`. The audit
    /// operation is recorded either way.
    fn reservation_response(
        request_id: &str,
        async_flag: bool,
        operation_id: &str,
        action: &str,
        result: serde_json::Value,
    ) -> ServerResult<axum::response::Response> {
        if async_flag {
            Ok((
                StatusCode::ACCEPTED,
                request_id_header(request_id)?,
                axum::Json(OperationAccepted {
                    operation_id: operation_id.to_string(),
                    status: "accepted".to_string(),
                    links: Some(OperationLinks {
                        self_link: Some(format!("/v1/operations/{operation_id}")),
                    }),
                }),
            )
                .into_response())
        } else {
            Ok((
                request_id_header(request_id)?,
                axum::Json(ActionResult {
                    status: "succeeded".to_string(),
                    action: action.to_string(),
                    message: None,
                    result: Some(result),
                }),
            )
                .into_response())
        }
    }

    /// Parse a create/update request body into a [`RuntimeReservation`]. All
    /// structural validation (family/address agreement, match predicate, prefix
    /// form) happens in `RuntimeReservation::from_parts`.
    fn parse_reservation(req: &ReservationMutationRequest) -> ServerResult<RuntimeReservation> {
        let obj = &req.reservation;
        let ip = obj
            .get("ip")
            .and_then(|v| v.as_str())
            .ok_or_else(|| crate::models::ServerError::bad_request("reservation.ip is required"))?;
        let prefix = obj.get("prefix").and_then(|v| v.as_str());
        let network = obj
            .get("network")
            .and_then(|v| v.as_str())
            .map(String::from);
        let match_val = obj.get("match").ok_or_else(|| {
            crate::models::ServerError::bad_request("reservation.match is required")
        })?;
        RuntimeReservation::from_parts(&req.family, ip, prefix, network, &match_val.to_string())
            .map_err(|e| crate::models::ServerError::bad_request(e.to_string()))
    }

    /// Map a store conflict error to the right HTTP status.
    fn reservation_error_to_server(
        err: config::reservations::ReservationError,
    ) -> crate::models::ServerError {
        use config::reservations::ReservationError::*;
        match err {
            AddressExists | MatchExists => crate::models::ServerError::conflict(err.to_string()),
            NotFound => crate::models::ServerError::not_found(err.to_string()),
        }
    }

    /// Summarize the caller's auth context for the audit trail (never a secret).
    fn actor(auth: &crate::ApiAuth) -> Option<String> {
        auth.auth_methods().into_iter().next()
    }

    /// Generate a unique operation id. Combines hex nanoseconds with a
    /// process-lifetime counter so two operations minted within the same clock
    /// tick can't collide on the `operation_id` primary key.
    fn new_operation_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("op-{nanos:x}-{seq:x}")
    }

    /// Parse an optional JSON body, defaulting an empty body to `T::default()`.
    /// Malformed JSON becomes a `400` in the standard error envelope rather than
    /// axum's default (envelope-less) rejection.
    fn parse_optional_body<T: DeserializeOwned + Default>(bytes: &Bytes) -> ServerResult<T> {
        if bytes.is_empty() {
            Ok(T::default())
        } else {
            serde_json::from_slice(bytes).map_err(|e| {
                crate::models::ServerError::bad_request(format!("invalid request body: {e}"))
            })
        }
    }

    /// Parse a required JSON body into the standard error envelope on failure.
    fn parse_required_body<T: DeserializeOwned>(bytes: &Bytes) -> ServerResult<T> {
        serde_json::from_slice(bytes).map_err(|e| {
            crate::models::ServerError::bad_request(format!("invalid request body: {e}"))
        })
    }

    /// Insert a terminal (already-succeeded) audit record for a synchronous
    /// action and return its operation id.
    async fn record_sync_action<S: Storage>(
        ip_mgr: &IpManager<S>,
        action: &str,
        actor: Option<String>,
        input: serde_json::Value,
        result: serde_json::Value,
    ) -> ServerResult<String> {
        let now = SystemTime::now();
        let operation_id = new_operation_id();
        let record = OperationRecord {
            operation_id: operation_id.clone(),
            action: action.to_string(),
            status: OperationStatus::Succeeded,
            actor,
            input_summary: Some(input.to_string()),
            result: Some(result.to_string()),
            error_code: None,
            error_message: None,
            created_at: now,
            started_at: Some(now),
            completed_at: Some(now),
        };
        ip_mgr.insert_operation(&record).await?;
        Ok(operation_id)
    }

    /// Convert a stored [`OperationRecord`] into the API `Operation` shape.
    fn operation_to_json(record: OperationRecord) -> Operation {
        let to_rfc3339 = |t: SystemTime| DateTime::<Utc>::from(t).to_rfc3339();
        let error = record.error_code.as_ref().map(|code| {
            serde_json::json!({
                "code": code,
                "message": record.error_message.clone().unwrap_or_default(),
                // the originating request id is not retained; report the record id
                "request_id": record.operation_id.clone(),
            })
        });
        let result = record
            .result
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        Operation {
            operation_id: record.operation_id,
            action: record.action,
            status: record.status.as_str().to_string(),
            created_at: to_rfc3339(record.created_at),
            started_at: record.started_at.map(to_rfc3339),
            completed_at: record.completed_at.map(to_rfc3339),
            result,
            error,
        }
    }

    pub(crate) fn request_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        format!("{nanos:x}")
    }

    fn request_id_header(request_id: &str) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_str(request_id)?);
        Ok(headers)
    }

    pub(crate) fn authorize(headers: &HeaderMap, auth: &crate::ApiAuth) -> ServerResult<()> {
        // A verified client certificate satisfies auth on its own. The TLS layer
        // stamps this header from the connection's peer cert and always strips
        // any client-supplied value first, so it can't be spoofed.
        if headers
            .get(crate::MTLS_HEADER)
            .is_some_and(|value| value == "1")
        {
            return Ok(());
        }

        let Some(expected) = auth.bearer_token.as_deref() else {
            return if auth.allow_unauthenticated {
                Ok(())
            } else {
                Err(crate::models::ServerError::unauthorized(
                    "management API authentication is not configured",
                ))
            };
        };

        let Some(actual) = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return Err(crate::models::ServerError::unauthorized(
                "missing bearer token",
            ));
        };

        // constant-time comparison so response timing can't be used as an oracle
        // to recover the token byte-by-byte
        if bool::from(actual.as_bytes().ct_eq(expected.as_bytes())) {
            Ok(())
        } else {
            Err(crate::models::ServerError::unauthorized(
                "invalid bearer token",
            ))
        }
    }

    fn yaml_to_json(yaml: yaml_serde::Value) -> serde_json::Value {
        use serde_json::Value as Json;
        use yaml_serde::Value as Yaml;

        match yaml {
            Yaml::Null => Json::Null,
            Yaml::Bool(value) => Json::Bool(value),
            Yaml::Number(value) => {
                if let Some(value) = value.as_i64() {
                    Json::from(value)
                } else if let Some(value) = value.as_u64() {
                    Json::from(value)
                } else if let Some(value) = value.as_f64() {
                    serde_json::Number::from_f64(value)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                } else {
                    Json::Null
                }
            }
            Yaml::String(value) => Json::String(value),
            Yaml::Sequence(values) => Json::Array(values.into_iter().map(yaml_to_json).collect()),
            Yaml::Mapping(values) => {
                let mut object = serde_json::Map::new();
                for (key, value) in values {
                    let key = match key {
                        Yaml::String(value) => value,
                        Yaml::Number(value) => value.to_string(),
                        Yaml::Bool(value) => value.to_string(),
                        other => yaml_to_json(other).to_string(),
                    };
                    object.insert(key, yaml_to_json(value));
                }
                Json::Object(object)
            }
            Yaml::Tagged(value) => yaml_to_json(value.value),
        }
    }

    fn build_metrics_detailed(families: &[prometheus::proto::MetricFamily]) -> MetricsDetailed {
        let mut counters = BTreeMap::new();
        let mut gauges = BTreeMap::new();
        let mut histograms = BTreeMap::new();

        for family in families {
            let name = family.name().to_string();
            match family.get_field_type() {
                prometheus::proto::MetricType::COUNTER => {
                    counters.insert(name, metric_family_total(family));
                }
                prometheus::proto::MetricType::GAUGE => {
                    gauges.insert(name, metric_family_total(family));
                }
                prometheus::proto::MetricType::HISTOGRAM => {
                    histograms.insert(name, histogram_family_to_json(family));
                }
                _ => {}
            }
        }

        MetricsDetailed {
            summary: build_metrics_summary(families),
            counters,
            gauges,
            histograms,
        }
    }

    fn build_metrics_summary(families: &[prometheus::proto::MetricFamily]) -> MetricsSummary {
        UPTIME.set(START_TIME.elapsed().as_secs() as i64);
        MetricsSummary {
            uptime_seconds: metric_family_total_by_name(families, "uptime") as u64,
            in_flight: metric_family_total_by_name(families, "in_flight") as u64,
            dhcpv4: ProtocolMetricsSummary {
                messages_received: metric_family_total_by_name(families, "recv_type_counts") as u64,
                messages_sent: metric_family_total_by_name(families, "sent_type_counts") as u64,
                errors: 0,
            },
            dhcpv6: ProtocolMetricsSummary {
                messages_received: metric_family_total_by_name(families, "v6_recv_type_counts")
                    as u64,
                messages_sent: metric_family_total_by_name(families, "v6_sent_type_counts") as u64,
                errors: 0,
            },
        }
    }

    fn metric_family_total_by_name(
        families: &[prometheus::proto::MetricFamily],
        name: &str,
    ) -> f64 {
        families
            .iter()
            .find(|family| family.name() == name)
            .map(metric_family_total)
            .unwrap_or_default()
    }

    fn metric_family_total(family: &prometheus::proto::MetricFamily) -> f64 {
        family
            .get_metric()
            .iter()
            .map(|metric| match family.get_field_type() {
                prometheus::proto::MetricType::COUNTER => metric
                    .counter
                    .as_ref()
                    .map(|counter| counter.value())
                    .unwrap_or_default(),
                prometheus::proto::MetricType::GAUGE => metric
                    .gauge
                    .as_ref()
                    .map(|gauge| gauge.value())
                    .unwrap_or_default(),
                prometheus::proto::MetricType::HISTOGRAM => metric
                    .histogram
                    .as_ref()
                    .map(|histogram| histogram.get_sample_sum())
                    .unwrap_or_default(),
                _ => 0.0,
            })
            .sum()
    }

    fn histogram_family_to_json(family: &prometheus::proto::MetricFamily) -> Histogram {
        let mut count = 0;
        let mut sum = 0.0;
        let mut buckets = Vec::new();

        for metric in family.get_metric() {
            let Some(histogram) = metric.histogram.as_ref() else {
                continue;
            };
            count += histogram.get_sample_count();
            sum += histogram.get_sample_sum();
            buckets.extend(histogram.get_bucket().iter().map(|bucket| HistogramBucket {
                le: bucket.upper_bound(),
                count: bucket.cumulative_count(),
            }));
        }

        Histogram {
            count,
            sum,
            buckets,
        }
    }

    fn metric_family_to_json(family: prometheus::proto::MetricFamily) -> MetricFamily {
        MetricFamily {
            name: family.name().to_string(),
            metric_type: metric_type_name(family.get_field_type()).to_string(),
            help: non_empty_string(family.help()),
            unit: None,
            samples: family
                .get_metric()
                .iter()
                .flat_map(|metric| {
                    metric_to_samples(family.name(), family.get_field_type(), metric)
                })
                .collect(),
        }
    }

    fn metric_to_samples(
        family_name: &str,
        metric_type: prometheus::proto::MetricType,
        metric: &prometheus::proto::Metric,
    ) -> Vec<MetricSample> {
        let labels = metric
            .get_label()
            .iter()
            .map(|label| (label.name().to_string(), label.value().to_string()))
            .collect::<BTreeMap<_, _>>();
        let labels = if labels.is_empty() {
            None
        } else {
            Some(labels)
        };

        match metric_type {
            prometheus::proto::MetricType::COUNTER => vec![MetricSample {
                name: family_name.to_string(),
                labels,
                value: metric
                    .counter
                    .as_ref()
                    .map(|counter| counter.value())
                    .unwrap_or_default(),
            }],
            prometheus::proto::MetricType::GAUGE => vec![MetricSample {
                name: family_name.to_string(),
                labels,
                value: metric
                    .gauge
                    .as_ref()
                    .map(|gauge| gauge.value())
                    .unwrap_or_default(),
            }],
            prometheus::proto::MetricType::HISTOGRAM => {
                let Some(histogram) = metric.histogram.as_ref() else {
                    return Vec::new();
                };
                let mut samples = Vec::with_capacity(histogram.get_bucket().len() + 2);
                for bucket in histogram.get_bucket() {
                    let mut bucket_labels = labels.clone().unwrap_or_default();
                    bucket_labels.insert("le".to_string(), bucket.upper_bound().to_string());
                    samples.push(MetricSample {
                        name: format!("{family_name}_bucket"),
                        labels: Some(bucket_labels),
                        value: bucket.cumulative_count() as f64,
                    });
                }
                samples.push(MetricSample {
                    name: format!("{family_name}_sum"),
                    labels: labels.clone(),
                    value: histogram.get_sample_sum(),
                });
                samples.push(MetricSample {
                    name: format!("{family_name}_count"),
                    labels,
                    value: histogram.get_sample_count() as f64,
                });
                samples
            }
            _ => Vec::new(),
        }
    }

    fn metric_type_name(metric_type: prometheus::proto::MetricType) -> &'static str {
        match metric_type {
            prometheus::proto::MetricType::COUNTER => "counter",
            prometheus::proto::MetricType::GAUGE => "gauge",
            prometheus::proto::MetricType::HISTOGRAM => "histogram",
            prometheus::proto::MetricType::SUMMARY => "summary",
            prometheus::proto::MetricType::UNTYPED => "unknown",
        }
    }

    fn non_empty_string(value: &str) -> Option<String> {
        (!value.is_empty()).then(|| value.to_string())
    }

    pub(crate) async fn leases<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
    ) -> ServerResult<axum::Json<crate::models::Leases>> {
        authorize(&headers, &auth)?;
        use crate::models::{LeaseIp, LeaseNetworks, LeaseState, Leases};
        use ip_manager::State as S;

        let cfg = (*cfg).clone();
        let mut networks = ip_mgr
            .select_all()
            .await?
            .into_iter()
            .map(|lease| {
                let info = lease.as_ref();
                let ip = info.ip();
                let id = info.id().map(hex::encode);
                let secs = info.expires_at().duration_since(UNIX_EPOCH)?.as_secs();
                let network = info.network();
                let expires_at_epoch = secs;
                let expires_at_utc = DateTime::<Utc>::from_timestamp(
                    info.expires_at().duration_since(UNIX_EPOCH)?.as_secs() as i64,
                    0,
                )
                .context("failed to create UTC datetime")?
                .to_rfc3339();
                let lease_info = LeaseIp {
                    ip,
                    id: id.clone(),
                    expires_at_epoch,
                    expires_at_utc,
                };

                let netv4 = match network {
                    std::net::IpAddr::V4(ip) => ip,
                    std::net::IpAddr::V6(_) => {
                        // TODO
                        warn!("/v1/leases does not support not dynamic ipv6 at this time");
                        return Ok(None);
                    }
                };
                if let Some(net) = cfg.v4().network(netv4) {
                    Ok(match lease {
                        S::Leased(_) => Some((net, LeaseState::Leased(lease_info))),
                        S::Probated(_) => Some((net, LeaseState::Probated(lease_info))),
                        // TODO if we store reserved in db, change this
                        S::Reserved(_) => None,
                    })
                } else {
                    Err(anyhow::anyhow!(
                        "failed to find network in cfg for {lease_info:?}"
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .fold(
                HashMap::<Ipv4Net, LeaseNetworks>::new(),
                |mut map, (net, lease)| {
                    let entry = map.entry(net.full_subnet()).or_default();
                    entry.ips.push(lease);

                    map
                },
            );
        // add reserved entries from config
        // TODO if we start to store reserved in db, then delete this
        for net in cfg.v4().networks().values() {
            for reservation in net.get_reservations() {
                let entry = networks.entry(net.full_subnet()).or_default();
                entry.ips.push(LeaseState::Reserved(ReserveIp {
                    ip: reservation.ip().into(),
                    id: None,
                    condition: reservation.condition().clone(),
                }))
            }
        }

        Ok(axum::Json(Leases { networks }))
    }

    pub(crate) async fn leases_v4<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Query(query): Query<LeaseListQuery>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let cfg = (*cfg).clone();
        let mut items = ip_mgr
            .select_all()
            .await?
            .into_iter()
            .filter_map(|lease| v4_lease_from_state(&cfg, lease).transpose())
            .collect::<Result<Vec<_>, _>>()?;

        add_v4_config_reservations(&cfg, &mut items);
        let items = filter_and_sort(items, &query)?;

        let (meta, items) = paginate(items, &query);
        Ok(axum::Json(V4LeaseListResponse { meta, items }))
    }

    pub(crate) async fn leases_v6<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Query(query): Query<LeaseListQuery>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let cfg = (*cfg).clone();
        let items = ip_mgr
            .select_all()
            .await?
            .into_iter()
            .filter_map(|lease| v6_lease_from_state(&cfg, lease).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        let items = filter_and_sort(items, &query)?;

        let (meta, items) = paginate(items, &query);
        Ok(axum::Json(V6LeaseListResponse { meta, items }))
    }

    pub(crate) async fn reservations_v4(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(reservations): Extension<RuntimeReservations>,
        Query(query): Query<LeaseListQuery>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let mut items = Vec::new();
        // Runtime reservations first — they take precedence over config, so a
        // config reservation for the same address is shadowed and omitted.
        let mut runtime_ips = HashSet::new();
        for res in reservations.list() {
            if res.family() != "v4" {
                continue;
            }
            if let config::reservations::ResMatch::V4(cond) = &res.match_ {
                runtime_ips.insert(res.ip_string());
                items.push(V4Reservation {
                    family: "v4".to_string(),
                    ip: res.ip_string(),
                    network: res.network.clone(),
                    source: "runtime".to_string(),
                    match_on: cond.clone(),
                });
            }
        }
        for net in cfg.v4().networks().values() {
            let network = net.full_subnet().to_string();
            for res in net.get_reservations() {
                if runtime_ips.contains(&res.ip().to_string()) {
                    continue;
                }
                items.push(V4Reservation {
                    family: "v4".to_string(),
                    ip: res.ip().to_string(),
                    network: Some(network.clone()),
                    source: "config".to_string(),
                    match_on: res.condition().clone(),
                });
            }
        }
        // filters
        if let Some(network) = query.network.as_deref() {
            items.retain(|r| r.network.as_deref() == Some(network));
        }
        if let Some(ip) = query.ip.as_deref() {
            items.retain(|r| r.ip == ip);
        }
        if let Some(client_id) = query.client_id.as_deref() {
            items.retain(|r| reservation_match_id(&r.match_on).as_deref() == Some(client_id));
        }
        items.sort_by(|a, b| a.ip.cmp(&b.ip));

        let (meta, items) = paginate(items, &query);
        Ok((
            request_id_header(&request_id)?,
            axum::Json(V4ReservationListResponse { meta, items }),
        ))
    }

    /// DHCPv6 reservations are runtime-only today (config has no v6 host
    /// reservations); list them from the runtime store.
    pub(crate) async fn reservations_v6(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(reservations): Extension<RuntimeReservations>,
        Query(query): Query<LeaseListQuery>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let mut items = Vec::new();
        for res in reservations.list() {
            if res.family() != "v6" {
                continue;
            }
            items.push(V6Reservation {
                family: "v6".to_string(),
                ip: Some(res.ip_string()),
                prefix: res.prefix_string(),
                network: res.network.clone(),
                source: "runtime".to_string(),
                match_on: res.match_value(),
            });
        }
        // filters
        if let Some(network) = query.network.as_deref() {
            items.retain(|r| r.network.as_deref() == Some(network));
        }
        if let Some(ip) = query.ip.as_deref() {
            items.retain(|r| r.ip.as_deref() == Some(ip));
        }
        if let Some(client_id) = query.client_id.as_deref() {
            // v6 reservations match on DUID; compare its hex form
            items.retain(|r| r.match_on.get("duid").and_then(|d| d.as_str()) == Some(client_id));
        }
        items.sort_by(|a, b| a.ip.cmp(&b.ip));

        let (meta, items) = paginate(items, &query);
        Ok((
            request_id_header(&request_id)?,
            axum::Json(V6ReservationListResponse { meta, items }),
        ))
    }

    /// The identifier a reservation matches on, for the `client_id` filter: the
    /// hex chaddr for a MAC match (matching how lease `client_id` is encoded),
    /// else `None`.
    fn reservation_match_id(condition: &Condition) -> Option<String> {
        match condition {
            Condition::Mac(mac) => Some(mac.to_string().replace([':', '-'], "").to_lowercase()),
            Condition::Options(_) => None,
        }
    }

    #[derive(Debug, Default, Deserialize)]
    pub(crate) struct LeaseListQuery {
        pub(crate) limit: Option<usize>,
        pub(crate) offset: Option<usize>,
        pub(crate) state: Option<String>,
        pub(crate) network: Option<String>,
        pub(crate) ip: Option<String>,
        pub(crate) client_id: Option<String>,
        pub(crate) expires_from: Option<String>,
        pub(crate) expires_to: Option<String>,
        pub(crate) sort: Option<String>,
    }

    fn v4_lease_from_state(
        cfg: &DhcpConfig,
        lease: ip_manager::State,
    ) -> anyhow::Result<Option<V4Lease>> {
        let info = lease.as_ref();
        let std::net::IpAddr::V4(ip) = info.ip() else {
            return Ok(None);
        };
        let network = match info.network() {
            std::net::IpAddr::V4(network) => cfg
                .v4()
                .network(network)
                .map(|network| network.full_subnet().to_string())
                .unwrap_or_else(|| network.to_string()),
            std::net::IpAddr::V6(network) => network.to_string(),
        };

        Ok(Some(V4Lease {
            family: "v4".to_string(),
            state: lease_state_name(&lease).to_string(),
            ip: ip.to_string(),
            network,
            client_id: info.id().map(hex::encode),
            expires_at: Some(expires_at_rfc3339(info.expires_at())?),
            source: Some("database".to_string()),
        }))
    }

    fn v6_lease_from_state(
        cfg: &DhcpConfig,
        lease: ip_manager::State,
    ) -> anyhow::Result<Option<V6Lease>> {
        let info = lease.as_ref();
        let std::net::IpAddr::V6(ip) = info.ip() else {
            return Ok(None);
        };
        let network = if cfg.has_v6() {
            cfg.v6()
                .get_network_by_addr(match info.network() {
                    std::net::IpAddr::V6(network) => network,
                    std::net::IpAddr::V4(_) => ip,
                })
                .map(|network| network.full_subnet().to_string())
                .unwrap_or_else(|| info.network().to_string())
        } else {
            info.network().to_string()
        };

        Ok(Some(V6Lease {
            family: "v6".to_string(),
            state: lease_state_name(&lease).to_string(),
            lease_type: "ia_na".to_string(),
            ip: Some(ip.to_string()),
            prefix: None,
            network,
            client_id: info.id().map(hex::encode),
            iaid: None,
            expires_at: Some(expires_at_rfc3339(info.expires_at())?),
            source: Some("database".to_string()),
        }))
    }

    fn add_v4_config_reservations(cfg: &DhcpConfig, items: &mut Vec<V4Lease>) {
        for net in cfg.v4().networks().values() {
            for reservation in net.get_reservations() {
                items.push(V4Lease {
                    family: "v4".to_string(),
                    state: "reserved".to_string(),
                    ip: reservation.ip().to_string(),
                    network: net.full_subnet().to_string(),
                    client_id: None,
                    expires_at: None,
                    source: Some("config".to_string()),
                });
            }
        }
    }

    fn lease_state_name(lease: &ip_manager::State) -> &'static str {
        match lease {
            ip_manager::State::Leased(_) => "leased",
            ip_manager::State::Probated(_) => "probated",
            ip_manager::State::Reserved(_) => "reserved",
        }
    }

    fn expires_at_rfc3339(expires_at: std::time::SystemTime) -> anyhow::Result<String> {
        let secs = expires_at.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        DateTime::<Utc>::from_timestamp(secs, 0)
            .context("failed to create UTC datetime")
            .map(|dt| dt.to_rfc3339())
    }

    /// Fields a lease row exposes for filtering and sorting. Implemented for
    /// both `V4Lease` and `V6Lease` so the query logic is shared.
    pub(crate) trait LeaseRow {
        fn state(&self) -> &str;
        fn ip(&self) -> Option<&str>;
        fn network(&self) -> &str;
        fn client_id(&self) -> Option<&str>;
        fn expires_at(&self) -> Option<&str>;
    }

    impl LeaseRow for V4Lease {
        fn state(&self) -> &str {
            &self.state
        }
        fn ip(&self) -> Option<&str> {
            Some(&self.ip)
        }
        fn network(&self) -> &str {
            &self.network
        }
        fn client_id(&self) -> Option<&str> {
            self.client_id.as_deref()
        }
        fn expires_at(&self) -> Option<&str> {
            self.expires_at.as_deref()
        }
    }

    impl LeaseRow for V6Lease {
        fn state(&self) -> &str {
            &self.state
        }
        fn ip(&self) -> Option<&str> {
            self.ip.as_deref()
        }
        fn network(&self) -> &str {
            &self.network
        }
        fn client_id(&self) -> Option<&str> {
            self.client_id.as_deref()
        }
        fn expires_at(&self) -> Option<&str> {
            self.expires_at.as_deref()
        }
    }

    /// A parsed sort key: field name plus direction.
    struct SortKey {
        field: String,
        desc: bool,
    }

    fn parse_sort(sort: Option<&str>) -> Vec<SortKey> {
        sort.unwrap_or("ip")
            .split(',')
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(|f| match f.strip_prefix('-') {
                Some(rest) => SortKey {
                    field: rest.to_string(),
                    desc: true,
                },
                None => SortKey {
                    field: f.to_string(),
                    desc: false,
                },
            })
            .collect()
    }

    /// Apply the query's filters and multi-field sort to a list of lease rows.
    /// A bad `expires_from`/`expires_to` date yields a 400 rather than being
    /// silently ignored.
    pub(crate) fn filter_and_sort<T: LeaseRow>(
        mut items: Vec<T>,
        query: &LeaseListQuery,
    ) -> ServerResult<Vec<T>> {
        let parse_bound =
            |name: &str, value: &Option<String>| -> ServerResult<Option<DateTime<Utc>>> {
                value
                    .as_deref()
                    .map(|raw| {
                        DateTime::parse_from_rfc3339(raw)
                            .map(|dt| dt.with_timezone(&Utc))
                            .map_err(|err| {
                                crate::models::ServerError::bad_request(format!(
                                    "invalid `{name}` timestamp (RFC 3339 expected): {err}"
                                ))
                            })
                    })
                    .transpose()
            };
        let expires_from = parse_bound("expires_from", &query.expires_from)?;
        let expires_to = parse_bound("expires_to", &query.expires_to)?;

        items.retain(|row| {
            if let Some(state) = query.state.as_deref()
                && !row.state().eq_ignore_ascii_case(state)
            {
                return false;
            }
            if let Some(ip) = query.ip.as_deref()
                && row.ip() != Some(ip)
            {
                return false;
            }
            if let Some(network) = query.network.as_deref()
                && row.network() != network
            {
                return false;
            }
            if let Some(client_id) = query.client_id.as_deref()
                && row.client_id() != Some(client_id)
            {
                return false;
            }
            if expires_from.is_some() || expires_to.is_some() {
                // rows with no expiry (config reservations) can't match a time window
                let Some(exp) = row
                    .expires_at()
                    .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                    .map(|dt| dt.with_timezone(&Utc))
                else {
                    return false;
                };
                if expires_from.is_some_and(|from| exp < from) {
                    return false;
                }
                if expires_to.is_some_and(|to| exp > to) {
                    return false;
                }
            }
            true
        });

        let keys = parse_sort(query.sort.as_deref());
        items.sort_by(|a, b| {
            for key in &keys {
                let ord = match key.field.as_str() {
                    "state" => a.state().cmp(b.state()),
                    "expires_at" => a.expires_at().cmp(&b.expires_at()),
                    "ip" => a.ip().cmp(&b.ip()),
                    _ => std::cmp::Ordering::Equal,
                };
                let ord = if key.desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });

        Ok(items)
    }

    fn paginate<T>(items: Vec<T>, query: &LeaseListQuery) -> (PaginationMeta, Vec<T>) {
        let limit = query.limit.unwrap_or(100).clamp(1, 1000);
        let offset = query.offset.unwrap_or_default();
        let total = items.len();
        let sort = query
            .sort
            .as_deref()
            .unwrap_or("ip")
            .split(',')
            .filter(|field| !field.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let filters = [
            ("state", query.state.as_deref()),
            ("network", query.network.as_deref()),
            ("ip", query.ip.as_deref()),
            ("client_id", query.client_id.as_deref()),
            ("expires_from", query.expires_from.as_deref()),
            ("expires_to", query.expires_to.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_string(), value.to_string())))
        .collect();
        let items = items
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let count = items.len();

        (
            PaginationMeta {
                limit,
                offset,
                total,
                count,
                filters,
                sort,
            },
            items,
        )
    }

    pub(crate) async fn config<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        // TODO: if serializing worked we could get DhcpConfig back into JSON/YAML but there's
        // a lot of logic left to make that particular transform. So just read from disk
        let path = cfg.path().context("no path specified for config")?;
        let raw = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to find config at {}", path.display()))?;
        // SECURITY: the config file contains DDNS TSIG key material. This endpoint
        // may be unauthenticated, so the raw file must never be returned. Parse it
        // into the typed wire config, blank out every secret, and re-serialize. If
        // it cannot be parsed we return an error rather than risk leaking secrets.
        let redacted = redact_config(&raw).context("failed to render config for display")?;
        // the active version is the id of the activated candidate, if any
        let version = ip_mgr
            .active_config_candidate()
            .await?
            .map(|c| c.candidate_id)
            .unwrap_or_else(|| "bootstrap".to_string());
        Ok((
            request_id_header(&request_id)?,
            axum::Json(ConfigDocument {
                version,
                redacted: true,
                document: serde_json::to_value(redacted)?,
            }),
        ))
    }

    /// Value substituted for any secret we strip out of the config before display.
    const REDACTED: &str = "**REDACTED**";

    /// Parse `raw` (YAML or JSON) into the typed wire config, replace all TSIG key
    /// material with [`REDACTED`], and return the typed config.
    /// Returns `Err` if the config cannot be parsed so a failure can never fall
    /// back to echoing the raw (secret-bearing) file.
    pub(crate) fn redact_config(raw: &str) -> anyhow::Result<config::wire::Config> {
        // Mirror the server's own loader (config::DhcpConfig::new), which tries
        // JSON first and then YAML. yaml_serde alone is not enough: it rejects
        // some inputs serde_json accepts (e.g. tab-indented JSON), which would
        // make /config return 500 for a JSON config that otherwise boots fine.
        let mut cfg: config::wire::Config = serde_json::from_str(raw)
            .or_else(|_| yaml_serde::from_str(raw))
            .context("could not parse config")?;
        if let Some(ddns) = cfg.v4.ddns.as_mut() {
            for key in ddns.tsig_keys.values_mut() {
                key.data = REDACTED.to_string();
            }
        }
        Ok(cfg)
    }

    // ---- config lifecycle -------------------------------------------------

    /// grace before an activate/rollback/reload restarts the process, so the
    /// `202` response can flush first
    const CONFIG_RESTART_GRACE: Duration = Duration::from_secs(2);

    /// Require privileged auth for config-lifecycle writes: a verified client
    /// cert (mTLS) when a client-CA is configured (production / GitOps), else the
    /// Bearer token (dev / no client-CA). This is how config pushes are gated to
    /// the GitOps orchestrator's certificate in production.
    fn authorize_privileged(headers: &HeaderMap, auth: &crate::ApiAuth) -> ServerResult<()> {
        if auth.mtls_enabled {
            if headers.get(crate::MTLS_HEADER).is_some_and(|v| v == "1") {
                Ok(())
            } else {
                Err(crate::models::ServerError::forbidden(
                    "this endpoint requires client-certificate (mTLS) authentication",
                ))
            }
        } else {
            authorize(headers, auth)
        }
    }

    fn new_candidate_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|e| e.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("cfg-{nanos:x}-{seq:x}")
    }

    /// Validate a candidate document by parsing it as a `DhcpConfig`. Returns the
    /// status (`valid`/`invalid`), any validation messages, and the text to
    /// persist / write to disk on activation.
    fn validate_document(document: &serde_json::Value) -> (String, Vec<ValidationMessage>, String) {
        let text = serde_json::to_string_pretty(document).unwrap_or_default();
        match config::DhcpConfig::parse_str(&text) {
            Ok(_) => ("valid".to_string(), Vec::new(), text),
            Err(err) => (
                "invalid".to_string(),
                vec![ValidationMessage {
                    level: "error".to_string(),
                    path: None,
                    message: format!("{err:#}"),
                }],
                text,
            ),
        }
    }

    /// Map a stored candidate to the API shape. The document is redacted (and
    /// omitted if it can't be parsed) so secrets are never returned.
    fn candidate_to_json(record: ConfigCandidateRecord, include_document: bool) -> ConfigCandidate {
        let to_rfc3339 = |t: SystemTime| DateTime::<Utc>::from(t).to_rfc3339();
        let validation: Vec<ValidationMessage> = record
            .validation
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let document = if include_document {
            redact_config(&record.document)
                .ok()
                .and_then(|c| serde_json::to_value(c).ok())
        } else {
            None
        };
        ConfigCandidate {
            candidate_id: record.candidate_id,
            status: record.status,
            created_at: to_rfc3339(record.created_at),
            activated_at: record.activated_at.map(to_rfc3339),
            message: record.message,
            validation,
            document,
        }
    }

    /// Atomically replace `path`'s contents (write a sibling temp file, fsync,
    /// rename) so a reader never sees a half-written config. Resolves a symlinked
    /// path so the real file is replaced (not the link), preserves the existing
    /// file's permissions so a secret-bearing config isn't widened, and cleans up
    /// the temp file on failure.
    fn atomic_write(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        // follow a symlink (e.g. a k8s mount) to the real file, so rename doesn't
        // replace the link with a plain file; if it doesn't exist yet, use as-is
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let dir = target.parent().unwrap_or_else(|| std::path::Path::new("."));
        let tmp = dir.join(format!(".{}.tmp", new_candidate_id()));

        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            // preserve the existing config's permissions — never widen a file that
            // may hold TSIG key material
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(&target) {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
            }
            std::fs::rename(&tmp, &target)
        };

        let result = write();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Write the candidate's document to the config file atomically and record
    /// the activation in the DB (superseding the previous active candidate).
    /// Serializes config writes (activate / rollback) process-wide, so two
    /// concurrent activations can't interleave the file write and the DB
    /// active-marker update (single-writer behavior).
    static CONFIG_WRITE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    async fn activate<S: Storage>(
        ip_mgr: &IpManager<S>,
        cfg: &DhcpConfig,
        candidate: &ConfigCandidateRecord,
    ) -> ServerResult<()> {
        let _guard = CONFIG_WRITE_LOCK.lock().await;
        let path = cfg.path().ok_or_else(|| {
            crate::models::ServerError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                anyhow::anyhow!("server has no config file path"),
            )
        })?;
        // re-validate immediately before writing, so we never persist a config
        // the server can't boot from. NOTE: this is parse-level validation (the
        // same as the on-disk loader) — it does not bind interfaces/sockets, so a
        // syntactically valid config that fails at that layer would surface on the
        // restart; a full dry-run boot is a possible follow-up.
        config::DhcpConfig::parse_str(&candidate.document).map_err(|err| {
            crate::models::ServerError::conflict(format!("candidate no longer parses: {err:#}"))
        })?;
        atomic_write(path, candidate.document.as_bytes()).map_err(|err| {
            crate::models::ServerError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                anyhow::anyhow!(err),
            )
        })?;
        // supersede the previous active candidate and mark this one activated in
        // one transaction, so the single active marker is never split
        ip_mgr
            .activate_config_candidate(&candidate.candidate_id, SystemTime::now())
            .await?;
        Ok(())
    }

    /// Record an accepted operation and schedule a graceful restart (so the
    /// datapath adopts the freshly-written config). Returns the operation id.
    async fn schedule_config_restart<S: Storage>(
        ip_mgr: &Arc<IpManager<S>>,
        shutdown: CancellationToken,
        action: &str,
        actor: Option<String>,
        input: serde_json::Value,
    ) -> ServerResult<String> {
        let operation_id = new_operation_id();
        let now = SystemTime::now();
        let record = OperationRecord {
            operation_id: operation_id.clone(),
            action: action.to_string(),
            status: OperationStatus::Accepted,
            actor,
            input_summary: Some(input.to_string()),
            result: None,
            error_code: None,
            error_message: None,
            created_at: now,
            started_at: None,
            completed_at: None,
        };
        ip_mgr.insert_operation(&record).await?;
        let ip_mgr = Arc::clone(ip_mgr);
        tokio::spawn(async move {
            let mut record = record;
            record.status = OperationStatus::Running;
            record.started_at = Some(SystemTime::now());
            let _ = ip_mgr.update_operation(&record).await;
            tokio::time::sleep(CONFIG_RESTART_GRACE).await;
            record.status = OperationStatus::Succeeded;
            record.completed_at = Some(SystemTime::now());
            record.result = Some(serde_json::json!({ "restarted": true }).to_string());
            let _ = ip_mgr.update_operation(&record).await;
            shutdown.cancel();
        });
        Ok(operation_id)
    }

    /// `PUT /v1/config` / `POST /v1/config/candidates` — stage + validate a
    /// candidate configuration.
    pub(crate) async fn create_config_candidate<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize_privileged(&headers, &auth)?;
        let request_id = request_id();
        let req: ConfigUpdateRequest = parse_required_body(&body)?;
        let (status, validation, text) = validate_document(&req.document);
        let candidate_id = new_candidate_id();
        let record = ConfigCandidateRecord {
            candidate_id: candidate_id.clone(),
            status: status.clone(),
            document: text,
            message: req.message,
            validation: Some(serde_json::to_string(&validation).unwrap_or_default()),
            created_at: SystemTime::now(),
            activated_at: None,
        };
        ip_mgr.upsert_config_candidate(&record).await?;
        // best-effort audit trail
        let _ = record_sync_action(
            &ip_mgr,
            "stage-config",
            actor(&auth),
            serde_json::json!({ "candidate_id": candidate_id }),
            serde_json::json!({ "candidate_id": candidate_id, "status": status }),
        )
        .await;
        Ok((
            StatusCode::ACCEPTED,
            request_id_header(&request_id)?,
            axum::Json(OperationAccepted {
                operation_id: candidate_id.clone(),
                status: "accepted".to_string(),
                links: Some(OperationLinks {
                    self_link: Some(format!("/v1/config/candidates/{candidate_id}")),
                }),
            }),
        )
            .into_response())
    }

    /// `GET /v1/config/candidates` — list candidates (newest first, documents omitted).
    pub(crate) async fn list_config_candidates<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Query(query): Query<LeaseListQuery>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let items: Vec<ConfigCandidate> = ip_mgr
            .list_config_candidates()
            .await?
            .into_iter()
            .map(|r| candidate_to_json(r, false))
            .collect();
        let (meta, items) = paginate(items, &query);
        Ok((
            request_id_header(&request_id)?,
            axum::Json(ConfigCandidateListResponse { meta, items }),
        ))
    }

    /// `GET /v1/config/candidates/{candidate_id}` — one candidate (redacted document).
    pub(crate) async fn get_config_candidate<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Path(candidate_id): Path<String>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let record = ip_mgr
            .get_config_candidate(&candidate_id)
            .await?
            .ok_or_else(|| {
                crate::models::ServerError::not_found(format!("candidate {candidate_id} not found"))
            })?;
        Ok((
            request_id_header(&request_id)?,
            axum::Json(candidate_to_json(record, true)),
        ))
    }

    /// `POST /v1/actions/activate-config` — activate a valid candidate (writes the
    /// config file, records history, restarts to adopt it).
    pub(crate) async fn activate_config<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(shutdown): Extension<CancellationToken>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize_privileged(&headers, &auth)?;
        let request_id = request_id();
        let req: ActivateConfigRequest = parse_required_body(&body)?;
        let candidate = ip_mgr
            .get_config_candidate(&req.candidate_id)
            .await?
            .ok_or_else(|| {
                crate::models::ServerError::not_found(format!(
                    "candidate {} not found",
                    req.candidate_id
                ))
            })?;
        if candidate.status == "invalid" {
            return Err(crate::models::ServerError::conflict(
                "cannot activate an invalid candidate",
            ));
        }
        activate(&ip_mgr, &cfg, &candidate).await?;
        let operation_id = schedule_config_restart(
            &ip_mgr,
            shutdown,
            "activate-config",
            actor(&auth),
            serde_json::json!({ "candidate_id": req.candidate_id }),
        )
        .await?;
        Ok((
            StatusCode::ACCEPTED,
            request_id_header(&request_id)?,
            axum::Json(OperationAccepted {
                operation_id: operation_id.clone(),
                status: "accepted".to_string(),
                links: Some(OperationLinks {
                    self_link: Some(format!("/v1/operations/{operation_id}")),
                }),
            }),
        )
            .into_response())
    }

    /// `POST /v1/actions/rollback-config` — re-activate a previously-activated
    /// version (candidate id).
    pub(crate) async fn rollback_config<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(shutdown): Extension<CancellationToken>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize_privileged(&headers, &auth)?;
        let request_id = request_id();
        let req: RollbackConfigRequest = parse_required_body(&body)?;
        let candidate = ip_mgr
            .get_config_candidate(&req.version)
            .await?
            .ok_or_else(|| {
                crate::models::ServerError::not_found(format!("version {} not found", req.version))
            })?;
        if candidate.activated_at.is_none() {
            return Err(crate::models::ServerError::conflict(
                "version was never activated",
            ));
        }
        activate(&ip_mgr, &cfg, &candidate).await?;
        let operation_id = schedule_config_restart(
            &ip_mgr,
            shutdown,
            "rollback-config",
            actor(&auth),
            serde_json::json!({ "version": req.version }),
        )
        .await?;
        Ok((
            StatusCode::ACCEPTED,
            request_id_header(&request_id)?,
            axum::Json(OperationAccepted {
                operation_id: operation_id.clone(),
                status: "accepted".to_string(),
                links: Some(OperationLinks {
                    self_link: Some(format!("/v1/operations/{operation_id}")),
                }),
            }),
        )
            .into_response())
    }

    /// `POST /v1/actions/reload` — re-read the on-disk config by restarting (after
    /// validating it still parses). Returns `202` (async) or `200`.
    pub(crate) async fn reload_config<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        Extension(shutdown): Extension<CancellationToken>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize_privileged(&headers, &auth)?;
        let request_id = request_id();
        let req: ReloadRequest = parse_optional_body(&body)?;
        // refuse to restart onto a broken on-disk config
        let path = cfg.path().ok_or_else(|| {
            crate::models::ServerError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                anyhow::anyhow!("server has no config file path"),
            )
        })?;
        let raw = std::fs::read_to_string(path).map_err(|err| {
            crate::models::ServerError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                anyhow::anyhow!(err),
            )
        })?;
        config::DhcpConfig::parse_str(&raw).map_err(|err| {
            crate::models::ServerError::conflict(format!("on-disk config is invalid: {err:#}"))
        })?;
        let operation_id = schedule_config_restart(
            &ip_mgr,
            shutdown,
            "reload",
            actor(&auth),
            serde_json::json!({}),
        )
        .await?;
        if req.r#async {
            Ok((
                StatusCode::ACCEPTED,
                request_id_header(&request_id)?,
                axum::Json(OperationAccepted {
                    operation_id: operation_id.clone(),
                    status: "accepted".to_string(),
                    links: Some(OperationLinks {
                        self_link: Some(format!("/v1/operations/{operation_id}")),
                    }),
                }),
            )
                .into_response())
        } else {
            Ok((
                request_id_header(&request_id)?,
                axum::Json(ActionResult {
                    status: "succeeded".to_string(),
                    action: "reload".to_string(),
                    message: Some("reloading configuration via graceful restart".to_string()),
                    result: None,
                }),
            )
                .into_response())
        }
    }

    // ---- lease / DDNS actions ---------------------------------------------

    /// `POST /v1/actions/release-lease` — release a lease from the store, and
    /// optionally remove its reverse (PTR) DNS record.
    pub(crate) async fn release_lease<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let req: ReleaseLeaseRequest = parse_required_body(&body)?;
        let ip: IpAddr = req.ip.parse().map_err(|_| {
            crate::models::ServerError::bad_request(format!("invalid ip {}", req.ip))
        })?;
        if !matches!(
            (req.family.as_str(), ip),
            ("v4", IpAddr::V4(_)) | ("v6", IpAddr::V6(_))
        ) {
            return Err(crate::models::ServerError::bad_request(
                "family does not match ip",
            ));
        }

        // resolve the client id: explicit, else the id of the lease at `ip`
        let id: Vec<u8> = match &req.client_id {
            Some(cid) => hex::decode(cid)
                .map_err(|_| crate::models::ServerError::bad_request("client_id must be hex"))?,
            None => ip_mgr
                .get(ip)
                .await?
                .as_ref()
                .and_then(|s| s.as_ref().id().map(|i| i.to_vec()))
                .ok_or_else(|| crate::models::ServerError::not_found("no lease at that address"))?,
        };

        let released = ip_mgr.release_ip(ip, &id).await?;
        if released.is_none() {
            return Err(crate::models::ServerError::not_found(
                "no matching lease to release",
            ));
        }

        // best-effort reverse DDNS cleanup (v4 only; forward needs the hostname)
        let mut ddns_status = "skipped";
        if req.ddns_cleanup
            && let IpAddr::V4(v4) = ip
            && let Some(ddns_cfg) = cfg.v4().ddns()
        {
            match ddns::apply(
                ddns_cfg,
                ddns::dhcid::DhcId::client_id(id.clone()),
                v4,
                Duration::ZERO,
                None,
                true,
            )
            .await
            {
                Ok(()) => ddns_status = "ok",
                Err(err) => {
                    warn!(?err, "DDNS cleanup on lease release failed");
                    ddns_status = "failed";
                }
            }
        }

        let result =
            serde_json::json!({ "family": req.family, "ip": req.ip, "ddns_cleanup": ddns_status });
        let operation_id = record_sync_action(
            &ip_mgr,
            "release-lease",
            actor(&auth),
            serde_json::json!({ "family": req.family, "ip": req.ip, "ddns_cleanup": req.ddns_cleanup }),
            result.clone(),
        )
        .await?;
        reservation_response(
            &request_id,
            req.r#async,
            &operation_id,
            "release-lease",
            result,
        )
    }

    /// `POST /v1/actions/trigger-ddns-update` — perform an out-of-band DDNS
    /// update or cleanup for an address (v4 only).
    pub(crate) async fn trigger_ddns<S: Storage>(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(ip_mgr): Extension<Arc<IpManager<S>>>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
        body: Bytes,
    ) -> ServerResult<axum::response::Response> {
        use std::str::FromStr;
        authorize(&headers, &auth)?;
        let request_id = request_id();
        let req: TriggerDdnsRequest = parse_required_body(&body)?;
        let cleanup = match req.operation.as_str() {
            "update" => false,
            "cleanup" => true,
            other => {
                return Err(crate::models::ServerError::bad_request(format!(
                    "operation must be 'update' or 'cleanup', got '{other}'"
                )));
            }
        };
        let ip: IpAddr = req.ip.parse().map_err(|_| {
            crate::models::ServerError::bad_request(format!("invalid ip {}", req.ip))
        })?;
        if !matches!(
            (req.family.as_str(), ip),
            ("v4", IpAddr::V4(_)) | ("v6", IpAddr::V6(_))
        ) {
            return Err(crate::models::ServerError::bad_request(
                "family does not match ip",
            ));
        }
        let IpAddr::V4(v4) = ip else {
            return Err(crate::models::ServerError::bad_request(
                "DDNS is only supported for v4 addresses",
            ));
        };
        let ddns_cfg = cfg
            .v4()
            .ddns()
            .ok_or_else(|| crate::models::ServerError::conflict("DDNS is not configured"))?;

        let domain = req
            .hostname
            .as_deref()
            .map(dora_core::dhcproto::Name::from_str)
            .transpose()
            .map_err(|_| crate::models::ServerError::bad_request("invalid hostname"))?;
        if !cleanup && domain.is_none() {
            return Err(crate::models::ServerError::bad_request(
                "hostname is required for a DDNS update",
            ));
        }
        // the DHCID is required whenever a forward name is targeted
        if domain.is_some() && req.client_id.is_none() {
            return Err(crate::models::ServerError::bad_request(
                "client_id is required when a hostname is given",
            ));
        }
        // Build the DHCID with the identifier type the server used at lease time
        // (defaults to client_id). Using the wrong type produces a DHCID that
        // won't match the record, so a forward cleanup would find nothing.
        let id_bytes = req
            .client_id
            .as_deref()
            .map(hex::decode)
            .transpose()
            .map_err(|_| crate::models::ServerError::bad_request("client_id must be hex"))?
            .unwrap_or_default();
        let id = match req.id_type.as_deref() {
            None | Some("client_id") => ddns::dhcid::DhcId::client_id(id_bytes),
            Some("chaddr") => ddns::dhcid::DhcId::chaddr(id_bytes),
            Some("duid") => ddns::dhcid::DhcId::duid(id_bytes),
            Some(other) => {
                return Err(crate::models::ServerError::bad_request(format!(
                    "id_type must be chaddr, client_id, or duid, got '{other}'"
                )));
            }
        };

        // manual DDNS updates use a default TTL (there is no lease context here)
        const DDNS_TTL: Duration = Duration::from_secs(3600);
        ddns::apply(ddns_cfg, id, v4, DDNS_TTL, domain, cleanup)
            .await
            .map_err(|err| {
                crate::models::ServerError::new(
                    StatusCode::BAD_GATEWAY,
                    "ddns_failed",
                    anyhow::anyhow!(err),
                )
            })?;

        let result = serde_json::json!({ "operation": req.operation, "status": "ok" });
        let operation_id = record_sync_action(
            &ip_mgr,
            "trigger-ddns-update",
            actor(&auth),
            serde_json::json!({ "operation": req.operation, "family": req.family, "ip": req.ip }),
            result.clone(),
        )
        .await?;
        reservation_response(
            &request_id,
            req.r#async,
            &operation_id,
            "trigger-ddns-update",
            result,
        )
    }

    pub(crate) async fn metrics(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        UPTIME.set(START_TIME.elapsed().as_secs() as i64);
        let encoder = ProtobufEncoder::new();
        let mut buf = Vec::new();
        let mf = prometheus::gather();
        let resp = Response::builder().header(header::CONTENT_TYPE, encoder.format_type());

        match encoder.encode(&mf, &mut buf) {
            Err(err) => {
                error!(?err, "error protobuf encoding prometheus metrics");
                Ok(resp
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())?)
            }
            Ok(_) => Ok(resp.status(StatusCode::OK).body(Body::from(buf))?),
        }
    }

    pub(crate) async fn metrics_text(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
    ) -> ServerResult<impl IntoResponse> {
        authorize(&headers, &auth)?;
        UPTIME.set(START_TIME.elapsed().as_secs() as i64);
        let encoder = TextEncoder::new();
        let mut buf = String::new();
        let mf = prometheus::gather();
        let resp = Response::builder().header(header::CONTENT_TYPE, encoder.format_type());

        match encoder.encode_utf8(&mf, &mut buf) {
            Err(err) => {
                error!(?err, "error text encoding prometheus metrics");
                Ok(resp
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())?)
            }
            Ok(_) => Ok(resp.status(StatusCode::OK).body(Body::from(buf))?),
        }
    }
}

/// Various models for API responses
pub mod models {
    use std::{collections::BTreeMap, collections::HashMap, fmt, net::IpAddr, sync::Arc};

    use axum::response::IntoResponse;
    use config::wire::v4::Condition;
    use ipnet::Ipv4Net;
    use parking_lot::Mutex;
    use serde::{Deserialize, Serialize};

    /// The overall health of the system
    pub type State = Arc<Mutex<Health>>;
    /// Health is binary Good/Bad at the moment
    #[derive(Serialize, Deserialize, Debug, PartialEq, Copy, Clone, Eq)]
    #[serde(rename_all = "UPPERCASE")]
    pub enum Health {
        /// Report good health
        Good,
        /// Report bad health
        Bad,
    }

    impl fmt::Display for Health {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "{}",
                match *self {
                    Health::Good => "GOOD",
                    Health::Bad => "BAD",
                }
            )
        }
    }

    /// Liveness response body.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct HealthResponse {
        /// Liveness state.
        pub status: String,
        /// Server-generated request id.
        pub request_id: String,
    }

    /// Readiness response body.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct ReadinessResponse {
        /// Readiness state.
        pub status: ReadinessStatus,
        /// Individual readiness checks.
        pub checks: Vec<ReadinessCheck>,
        /// Server-generated request id.
        pub request_id: String,
    }

    /// Readiness state.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Copy, Clone, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ReadinessStatus {
        /// Server is ready.
        Ready,
        /// Server is alive but not ready.
        NotReady,
    }

    /// Individual readiness check.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct ReadinessCheck {
        /// Check name.
        pub name: String,
        /// Check status.
        pub status: String,
        /// Optional human-readable detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
    }

    /// Summary metrics for health and dashboards.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct MetricsSummary {
        /// Server uptime in seconds.
        pub uptime_seconds: u64,
        /// Currently in-flight DHCP messages.
        pub in_flight: u64,
        /// DHCPv4 metric summary.
        pub dhcpv4: ProtocolMetricsSummary,
        /// DHCPv6 metric summary.
        pub dhcpv6: ProtocolMetricsSummary,
    }

    /// Protocol-specific metric summary.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct ProtocolMetricsSummary {
        /// Messages received.
        pub messages_received: u64,
        /// Messages sent.
        pub messages_sent: u64,
        /// Protocol errors.
        pub errors: u64,
    }

    /// Detailed structured metrics.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct MetricsDetailed {
        /// Summary metrics.
        pub summary: MetricsSummary,
        /// Counter metric family totals.
        pub counters: BTreeMap<String, f64>,
        /// Gauge metric family totals.
        pub gauges: BTreeMap<String, f64>,
        /// Histogram metric family totals.
        pub histograms: BTreeMap<String, Histogram>,
    }

    /// Histogram metric.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct Histogram {
        /// Sample count.
        pub count: u64,
        /// Sample sum.
        pub sum: f64,
        /// Histogram buckets.
        pub buckets: Vec<HistogramBucket>,
    }

    /// Histogram bucket.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct HistogramBucket {
        /// Upper bound.
        pub le: f64,
        /// Cumulative count.
        pub count: u64,
    }

    /// OpenMetrics-inspired JSON metric families.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct OpenMetricsJson {
        /// Metric families.
        pub families: Vec<MetricFamily>,
    }

    /// Metric family.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct MetricFamily {
        /// Family name.
        pub name: String,
        /// Family type.
        #[serde(rename = "type")]
        pub metric_type: String,
        /// Help text.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub help: Option<String>,
        /// Metric unit.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub unit: Option<String>,
        /// Samples in this family.
        pub samples: Vec<MetricSample>,
    }

    /// Metric sample.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct MetricSample {
        /// Sample name.
        pub name: String,
        /// Labels.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub labels: Option<BTreeMap<String, String>>,
        /// Sample value.
        pub value: f64,
    }

    /// Server metadata and runtime state.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct ServerInfo {
        /// Server instance id.
        pub id: String,
        /// Server version.
        pub version: String,
        /// Server start timestamp.
        pub started_at: String,
        /// Server mode.
        pub mode: ServerMode,
        /// API metadata.
        pub api: ServerApiInfo,
        /// Server-generated request id.
        pub request_id: String,
    }

    /// Server mode. The canonical definition lives in `dora-core` because the
    /// DHCP datapath also reads it to enforce drain / maintenance; re-exported
    /// here so it appears in this crate's public API and `ServerInfo`.
    pub use dora_core::mode::ServerMode;

    /// API metadata.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct ServerApiInfo {
        /// API version.
        pub version: String,
        /// Enabled authentication mechanisms.
        pub auth: Vec<String>,
    }

    /// Request body for `POST /v1/actions/maintenance-mode`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct MaintenanceModeRequest {
        /// Enter maintenance when true, leave (return to normal) when false.
        pub enabled: bool,
        /// Optional operator-supplied reason, recorded in the audit trail.
        #[serde(default)]
        pub reason: Option<String>,
    }

    /// Request body for `POST /v1/actions/drain` (all fields optional).
    #[derive(Deserialize, Debug, Default)]
    #[serde(deny_unknown_fields)]
    pub struct DrainRequest {
        /// Optional operator-supplied reason, recorded in the audit trail.
        #[serde(default)]
        pub reason: Option<String>,
    }

    /// Request body for `POST /v1/actions/shutdown` (all fields optional).
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct ShutdownRequest {
        /// How long to keep draining before the process exits. Defaults to 30s.
        #[serde(default = "default_grace_period")]
        pub grace_period_seconds: u64,
        /// Optional operator-supplied reason, recorded in the audit trail.
        #[serde(default)]
        pub reason: Option<String>,
    }

    impl Default for ShutdownRequest {
        fn default() -> Self {
            Self {
                grace_period_seconds: default_grace_period(),
                reason: None,
            }
        }
    }

    fn default_grace_period() -> u64 {
        30
    }

    /// Request body for `POST /v1/actions/{create,update}-reservation`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct ReservationMutationRequest {
        /// `v4` or `v6`.
        pub family: String,
        /// The reservation object (`V4Reservation` / `V6Reservation` shape):
        /// `ip`, optional `prefix`/`network`, and `match`.
        pub reservation: serde_json::Value,
        /// Return `202` with an operation to poll instead of `200`.
        #[serde(default, rename = "async")]
        pub r#async: bool,
    }

    /// Request body for `POST /v1/actions/delete-reservation`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct DeleteReservationRequest {
        /// `v4` or `v6`.
        pub family: String,
        /// The reserved address to delete.
        pub ip: String,
        /// Return `202` with an operation to poll instead of `200`.
        #[serde(default, rename = "async")]
        pub r#async: bool,
    }

    /// Synchronous action result (`ActionResult` in the contract). The request id
    /// is carried in the `X-Request-ID` header, not the body.
    #[derive(Serialize, Debug)]
    pub struct ActionResult {
        /// `succeeded` or `failed`.
        pub status: String,
        /// The action that ran, e.g. `drain`.
        pub action: String,
        /// Optional human-readable detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
        /// Optional structured result payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub result: Option<serde_json::Value>,
    }

    /// Body of a `202 Accepted` async action response (`OperationAccepted`).
    #[derive(Serialize, Debug)]
    pub struct OperationAccepted {
        /// The id to poll at `GET /v1/operations/{id}`.
        pub operation_id: String,
        /// Always `accepted`.
        pub status: String,
        /// Hypermedia links (the `self` operation URL).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub links: Option<OperationLinks>,
    }

    /// Links block of [`OperationAccepted`].
    #[derive(Serialize, Debug)]
    pub struct OperationLinks {
        /// URL of the operation resource.
        #[serde(rename = "self", skip_serializing_if = "Option::is_none")]
        pub self_link: Option<String>,
    }

    /// A persisted async operation record (`Operation` in the contract).
    #[derive(Serialize, Debug)]
    pub struct Operation {
        /// Operation id.
        pub operation_id: String,
        /// The action that produced it.
        pub action: String,
        /// `accepted`, `running`, `succeeded`, `failed`, or `canceled`.
        pub status: String,
        /// Creation timestamp (RFC 3339).
        pub created_at: String,
        /// When work began (RFC 3339), if it has.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub started_at: Option<String>,
        /// When work finished (RFC 3339), if it has.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub completed_at: Option<String>,
        /// Structured result payload on success.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub result: Option<serde_json::Value>,
        /// Error object on failure (`{ code, message, request_id }`).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub error: Option<serde_json::Value>,
    }

    /// Active redacted configuration document.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct ConfigDocument {
        /// Document version.
        pub version: String,
        /// Whether the document has been redacted.
        pub redacted: bool,
        /// Redacted configuration payload.
        pub document: serde_json::Value,
    }

    /// Request body for staging a config candidate (`PUT /v1/config`,
    /// `POST /v1/config/candidates`).
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct ConfigUpdateRequest {
        /// The proposed configuration document.
        pub document: serde_json::Value,
        /// Optional operator note recorded with the candidate.
        #[serde(default)]
        pub message: Option<String>,
    }

    /// A single validation finding for a candidate.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct ValidationMessage {
        /// `info`, `warning`, or `error`.
        pub level: String,
        /// Optional config path the finding refers to.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub path: Option<String>,
        /// Human-readable message.
        pub message: String,
    }

    /// A staged configuration candidate.
    #[derive(Serialize, Debug)]
    pub struct ConfigCandidate {
        /// Candidate id (also its version once activated).
        pub candidate_id: String,
        /// `staged`, `validating`, `valid`, `invalid`, `activated`, or `superseded`.
        pub status: String,
        /// Creation timestamp (RFC 3339).
        pub created_at: String,
        /// Activation timestamp (RFC 3339), if it has been activated.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub activated_at: Option<String>,
        /// Optional operator note.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
        /// Validation findings.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        pub validation: Vec<ValidationMessage>,
        /// The candidate document (redacted).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub document: Option<serde_json::Value>,
    }

    /// Paginated candidate list.
    #[derive(Serialize, Debug)]
    pub struct ConfigCandidateListResponse {
        /// Pagination metadata.
        pub meta: PaginationMeta,
        /// Candidates (newest first), documents omitted.
        pub items: Vec<ConfigCandidate>,
    }

    /// Request body for `POST /v1/actions/activate-config`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct ActivateConfigRequest {
        /// The candidate to activate.
        pub candidate_id: String,
    }

    /// Request body for `POST /v1/actions/rollback-config`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct RollbackConfigRequest {
        /// The previously-activated version (candidate id) to roll back to.
        pub version: String,
    }

    /// Request body for `POST /v1/actions/reload` (all fields optional).
    #[derive(Deserialize, Debug, Default)]
    #[serde(deny_unknown_fields)]
    pub struct ReloadRequest {
        /// Return `202` with an operation instead of `200`.
        #[serde(default, rename = "async")]
        pub r#async: bool,
    }

    /// Request body for `POST /v1/actions/release-lease`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct ReleaseLeaseRequest {
        /// `v4` or `v6`.
        pub family: String,
        /// The leased address to release.
        pub ip: String,
        /// Hex client identifier; if omitted, the lease at `ip` is released.
        #[serde(default)]
        pub client_id: Option<String>,
        /// Also remove the lease's reverse (PTR) DNS record (v4 only).
        #[serde(default)]
        pub ddns_cleanup: bool,
        /// Return `202` with an operation instead of `200`.
        #[serde(default, rename = "async")]
        pub r#async: bool,
    }

    /// Request body for `POST /v1/actions/trigger-ddns-update`.
    #[derive(Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    pub struct TriggerDdnsRequest {
        /// `update` (add A/PTR) or `cleanup` (remove them).
        pub operation: String,
        /// `v4` or `v6` (DDNS is v4-only today).
        pub family: String,
        /// The leased address.
        pub ip: String,
        /// The FQDN; required for `update` and for forward-zone `cleanup`.
        #[serde(default)]
        pub hostname: Option<String>,
        /// Hex client identifier; required when a `hostname` is given (DHCID).
        #[serde(default)]
        pub client_id: Option<String>,
        /// How to interpret `client_id` when forming the DHCID (must match what
        /// the server used at lease time): `chaddr`, `client_id` (default), or
        /// `duid`.
        #[serde(default)]
        pub id_type: Option<String>,
        /// Return `202` with an operation instead of `200`.
        #[serde(default, rename = "async")]
        pub r#async: bool,
    }

    /// Pagination metadata.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct PaginationMeta {
        /// Page limit.
        pub limit: usize,
        /// Page offset.
        pub offset: usize,
        /// Total records before pagination.
        pub total: usize,
        /// Records returned in this page.
        pub count: usize,
        /// Applied filters.
        pub filters: BTreeMap<String, String>,
        /// Applied sort fields.
        pub sort: Vec<String>,
    }

    /// DHCPv4 lease list response.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V4LeaseListResponse {
        /// Pagination metadata.
        pub meta: PaginationMeta,
        /// DHCPv4 leases.
        pub items: Vec<V4Lease>,
    }

    /// DHCPv6 lease list response.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V6LeaseListResponse {
        /// Pagination metadata.
        pub meta: PaginationMeta,
        /// DHCPv6 leases.
        pub items: Vec<V6Lease>,
    }

    /// DHCPv4 lease item.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V4Lease {
        /// Address family.
        pub family: String,
        /// Lease state.
        pub state: String,
        /// IPv4 address.
        pub ip: String,
        /// Owning network.
        pub network: String,
        /// Hex-encoded client identifier.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub client_id: Option<String>,
        /// Expiration timestamp.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub expires_at: Option<String>,
        /// Lease source.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub source: Option<String>,
    }

    /// DHCPv6 lease item.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V6Lease {
        /// Address family.
        pub family: String,
        /// Lease state.
        pub state: String,
        /// DHCPv6 binding type.
        pub lease_type: String,
        /// IPv6 address for IA_NA leases.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ip: Option<String>,
        /// Delegated prefix for IA_PD leases.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub prefix: Option<String>,
        /// Owning network.
        pub network: String,
        /// Hex-encoded client identifier.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub client_id: Option<String>,
        /// IAID.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub iaid: Option<u32>,
        /// Expiration timestamp.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub expires_at: Option<String>,
        /// Lease source.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub source: Option<String>,
    }

    /// leases table
    #[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone, Eq)]
    pub struct Leases {
        /// map of networks
        pub networks: HashMap<Ipv4Net, LeaseNetworks>,
    }

    /// list of leases
    #[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone, Eq)]
    pub struct LeaseNetworks {
        /// list of ips in database
        pub ips: Vec<LeaseState>,
    }

    /// lease state
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    #[serde(tag = "type", rename_all = "lowercase")]
    pub enum LeaseState {
        /// reserved
        Reserved(ReserveIp),
        /// leased
        Leased(LeaseIp),
        /// probated ip
        Probated(LeaseIp),
    }

    /// details about lease ip
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct LeaseIp {
        /// ip
        pub ip: IpAddr,
        /// id
        pub id: Option<String>,
        /// expiry as u64
        pub expires_at_epoch: u64,
        /// expiry as string
        pub expires_at_utc: String,
    }

    /// static reservation
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct ReserveIp {
        /// ip
        pub ip: IpAddr,
        /// id: will be None for now
        pub id: Option<String>,
        /// reservation condition
        #[serde(rename = "match")]
        pub condition: Condition,
    }

    /// DHCPv4 reservation list response.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V4ReservationListResponse {
        /// Pagination metadata.
        pub meta: PaginationMeta,
        /// DHCPv4 reservations.
        pub items: Vec<V4Reservation>,
    }

    /// DHCPv4 reservation item.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V4Reservation {
        /// Address family (always `v4`).
        pub family: String,
        /// Reserved IPv4 address.
        pub ip: String,
        /// Owning network.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub network: Option<String>,
        /// `config` (from the config file) or `runtime` (via the API).
        pub source: String,
        /// The match predicate (chaddr or options).
        #[serde(rename = "match")]
        pub match_on: Condition,
    }

    /// DHCPv6 reservation list response.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V6ReservationListResponse {
        /// Pagination metadata.
        pub meta: PaginationMeta,
        /// DHCPv6 reservations.
        pub items: Vec<V6Reservation>,
    }

    /// DHCPv6 reservation item.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct V6Reservation {
        /// Address family (always `v6`).
        pub family: String,
        /// Reserved IPv6 address, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ip: Option<String>,
        /// Reserved delegated prefix, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub prefix: Option<String>,
        /// Owning network.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub network: Option<String>,
        /// `config` or `runtime`.
        pub source: String,
        /// The match predicate.
        #[serde(rename = "match")]
        pub match_on: serde_json::Value,
    }

    pub(crate) fn blank_health() -> State {
        Arc::new(Mutex::new(Health::Bad))
    }

    // error type
    /// Make our own error that wraps `anyhow::Error`.
    #[derive(Debug)]
    pub struct ServerError {
        status: axum::http::StatusCode,
        /// stable, machine-readable error code (e.g. `unauthorized`, `internal`)
        code: &'static str,
        error: anyhow::Error,
        /// optional structured details, e.g. offending fields for a validation error
        details: Option<serde_json::Value>,
    }
    /// return error result
    pub type ServerResult<T> = Result<T, ServerError>;

    impl ServerError {
        pub(crate) fn new(
            status: axum::http::StatusCode,
            code: &'static str,
            error: anyhow::Error,
        ) -> Self {
            Self {
                status,
                code,
                error,
                details: None,
            }
        }
        pub(crate) fn unauthorized(message: &'static str) -> Self {
            Self::new(
                axum::http::StatusCode::UNAUTHORIZED,
                "unauthorized",
                anyhow::anyhow!(message),
            )
        }
        pub(crate) fn bad_request(message: impl Into<String>) -> Self {
            Self::new(
                axum::http::StatusCode::BAD_REQUEST,
                "bad_request",
                anyhow::anyhow!(message.into()),
            )
        }
        pub(crate) fn not_found(message: impl Into<String>) -> Self {
            Self::new(
                axum::http::StatusCode::NOT_FOUND,
                "not_found",
                anyhow::anyhow!(message.into()),
            )
        }
        pub(crate) fn conflict(message: impl Into<String>) -> Self {
            Self::new(
                axum::http::StatusCode::CONFLICT,
                "conflict",
                anyhow::anyhow!(message.into()),
            )
        }
        pub(crate) fn forbidden(message: impl Into<String>) -> Self {
            Self::new(
                axum::http::StatusCode::FORBIDDEN,
                "forbidden",
                anyhow::anyhow!(message.into()),
            )
        }
    }

    /// The standard error envelope: `{ "error": { code, message, request_id, details } }`.
    #[derive(Serialize)]
    struct ErrorEnvelope {
        error: ErrorBody,
    }

    #[derive(Serialize)]
    struct ErrorBody {
        code: &'static str,
        message: String,
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    }

    impl IntoResponse for ServerError {
        fn into_response(self) -> axum::response::Response {
            let request_id = crate::handlers::request_id();

            // SECURITY: 5xx errors carry internal detail (file paths, DB errors) in
            // the anyhow chain. Log it server-side but return a generic message so
            // we never leak filesystem/internal state to clients.
            let message = if self.status.is_server_error() {
                tracing::error!(code = self.code, error = ?self.error, "API internal error");
                "internal server error".to_string()
            } else {
                format!("{}", self.error)
            };

            let body = ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message,
                    request_id: request_id.clone(),
                    details: self.details,
                },
            };

            let mut response = (self.status, axum::Json(body)).into_response();
            if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
                response.headers_mut().insert("x-request-id", value);
            }
            response
        }
    }

    impl<E> From<E> for ServerError
    where
        E: Into<anyhow::Error>,
    {
        fn from(err: E) -> Self {
            Self::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                err.into(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        path::PathBuf,
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use dora_core::mode::{ServerMode, SharedMode};
    use ip_manager::postgres::PostgresDb;
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn auth_fails_closed_without_an_explicit_method() {
        let auth = ApiAuth {
            bearer_token: None,
            allow_unauthenticated: false,
            mtls_enabled: false,
        };
        let err = handlers::authorize(&axum::http::HeaderMap::new(), &auth)
            .expect_err("an unconfigured production API must reject protected routes");
        assert_eq!(
            axum::response::IntoResponse::into_response(err).status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    async fn spawn_test_api(health: Health) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        spawn_test_api_with_auth(health, ApiAuth::disabled()).await
    }

    async fn spawn_test_api_with_auth(
        health: Health,
        auth: ApiAuth,
    ) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        let (addr, token, _mode, _mgr) = spawn_test_api_full(health, auth).await?;
        Ok((addr, token))
    }

    /// Like [`spawn_test_api_with_auth`] but also hands back the shared mode
    /// handle and the `IpManager`, so operation/mode tests can drive and inspect
    /// them directly.
    #[allow(clippy::type_complexity)]
    async fn spawn_test_api_full(
        health: Health,
        auth: ApiAuth,
    ) -> anyhow::Result<(
        SocketAddr,
        CancellationToken,
        SharedMode,
        Arc<IpManager<PostgresDb>>,
    )> {
        let mgr = Arc::new(IpManager::new(PostgresDb::new_test().await?)?);
        let cfg = Arc::new(DhcpConfig::default());
        let state = models::blank_health();
        *state.lock() = health;
        let token = CancellationToken::new();
        let mode = SharedMode::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = api_router::<PostgresDb>(
            state,
            ApiState {
                started_at: SystemTime::now(),
            },
            auth,
            cfg,
            Arc::clone(&mgr),
            mode.clone(),
            RuntimeReservations::new(),
            token.clone(),
            Duration::from_secs(30),
        );
        let shutdown = token.clone();

        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown.cancelled().await;
                })
                .await
            {
                tracing::error!(?err, "test external API task returned error");
            }
        });

        Ok((addr, token, mode, mgr))
    }

    async fn spawn_test_api_with_config(
        health: Health,
        auth: ApiAuth,
        cfg: Arc<DhcpConfig>,
    ) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        let mgr = Arc::new(IpManager::new(PostgresDb::new_test().await?)?);
        let state = models::blank_health();
        *state.lock() = health;
        let token = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = api_router::<PostgresDb>(
            state,
            ApiState {
                started_at: SystemTime::now(),
            },
            auth,
            cfg,
            mgr,
            SharedMode::default(),
            RuntimeReservations::new(),
            token.clone(),
            Duration::from_secs(30),
        );
        let shutdown = token.clone();

        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown.cancelled().await;
                })
                .await
            {
                tracing::error!(?err, "test external API task returned error");
            }
        });

        Ok((addr, token))
    }

    // The /config endpoint can expose config material, so it must never leak the DDNS
    // TSIG secret. Verify the key material is stripped and the reservation
    // `match:` map form (which yaml_serde would otherwise reject) round-trips.
    #[test]
    fn test_redact_config_strips_tsig_secret() {
        let raw = r#"
v4:
  ddns:
    enable_updates: true
    forward: []
    reverse: []
    tsig_keys:
      key_foo:
        algorithm: hmac-sha256
        data: "SUPERSECRETKEYMATERIAL=="
  networks:
    192.168.0.0/24:
      ranges:
        - start: 192.168.0.100
          end: 192.168.0.200
          options:
            values: {}
          config:
            lease_time:
              default: 3600
      reservations:
        - ip: 192.168.0.50
          match:
            chaddr: aa:bb:cc:dd:ee:ff
          options:
            values: {}
"#;
        let out = crate::handlers::redact_config(raw).expect("redact should succeed");
        let out = serde_json::to_value(out).expect("serialize redacted config");
        assert!(
            !out.to_string().contains("SUPERSECRETKEYMATERIAL"),
            "TSIG secret leaked into /config output:\n{out}"
        );
        assert!(
            out.to_string().contains("**REDACTED**"),
            "expected redaction marker"
        );
    }

    // The server accepts JSON configs (and tries JSON before YAML at startup),
    // so /config must redact a JSON config too. Tab indentation is valid JSON
    // but invalid YAML, so this also guards the yaml-only-parse regression.
    #[test]
    fn test_redact_config_accepts_json() {
        let raw = "{\n\t\"v4\": {\n\t\t\"ddns\": {\n\t\t\t\"enable_updates\": true,\n\t\t\t\"forward\": [],\n\t\t\t\"reverse\": [],\n\t\t\t\"tsig_keys\": {\n\t\t\t\t\"key_foo\": { \"algorithm\": \"hmac-sha256\", \"data\": \"SUPERSECRETKEYMATERIAL==\" }\n\t\t\t}\n\t\t}\n\t}\n}";
        let out = crate::handlers::redact_config(raw).expect("json redact should succeed");
        let out = serde_json::to_value(out).expect("serialize redacted config");
        assert!(
            !out.to_string().contains("SUPERSECRETKEYMATERIAL"),
            "secret leaked:\n{out}"
        );
        assert!(out.to_string().contains("**REDACTED**"));
    }

    #[test]
    fn test_redact_config_rejects_unparseable() {
        // must error (not echo the raw file) when the config cannot be parsed
        assert!(crate::handlers::redact_config("this: is: not: valid").is_err());
    }

    #[tokio::test]
    async fn test_health() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Bad).await?;
        let response = reqwest::get(format!("http://{addr}/health"))
            .await?
            .error_for_status()?;
        let header_request_id = response
            .headers()
            .get("x-request-id")
            .expect("x-request-id header")
            .to_str()?
            .to_string();
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["status"], "alive");
        assert!(body["request_id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(body["request_id"], header_request_id);
        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_ready() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::get(format!("http://{addr}/ready"))
            .await?
            .error_for_status()?;
        let header_request_id = response
            .headers()
            .get("x-request-id")
            .expect("x-request-id header")
            .to_str()?
            .to_string();
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"][0]["name"], "health");
        assert_eq!(body["checks"][0]["status"], "pass");
        assert!(body["request_id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(body["request_id"], header_request_id);
        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_not_ready() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Bad).await?;
        let err = reqwest::get(format!("http://{addr}/ready"))
            .await?
            .error_for_status()
            .expect_err("not ready should return 503");
        assert_eq!(err.status(), Some(reqwest::StatusCode::SERVICE_UNAVAILABLE));
        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_openapi_json() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::get(format!("http://{addr}/openapi.json"))
            .await?
            .error_for_status()?;
        assert!(
            response.headers().get("x-request-id").is_some(),
            "expected x-request-id header"
        );
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["openapi"], "3.1.0");
        assert!(body["paths"]["/health"].is_object());
        assert!(body["paths"]["/ready"].is_object());
        assert!(body["paths"]["/openapi.json"].is_object());
        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_swagger_ui_docs_public() -> anyhow::Result<()> {
        // Configure a bearer token so `authorize()` actually enforces auth, then
        // hit the docs routes WITHOUT presenting it: they must serve, while a
        // gated route (`/v1/server`) must 401. This proves the docs handlers are
        // public by construction, not just because auth happens to be disabled.
        let (addr, token) =
            spawn_test_api_with_auth(Health::Good, ApiAuth::bearer("secret")).await?;

        let gated = reqwest::get(format!("http://{addr}/v1/server")).await?;
        assert_eq!(
            gated.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "a gated route must reject an unauthenticated request in this test setup"
        );

        let page = reqwest::get(format!("http://{addr}/docs"))
            .await?
            .error_for_status()?;
        assert_eq!(
            page.headers()[reqwest::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let html = page.text().await?;
        assert!(html.contains("swagger-ui"), "expected the Swagger UI shell");
        assert!(
            html.contains("/openapi.json"),
            "docs page should point Swagger UI at the spec"
        );

        let js = reqwest::get(format!("http://{addr}/docs/swagger-ui-bundle.js"))
            .await?
            .error_for_status()?;
        assert!(
            js.headers()[reqwest::header::CONTENT_TYPE]
                .to_str()?
                .contains("javascript")
        );
        assert!(!js.text().await?.is_empty(), "bundle should be non-empty");

        let css = reqwest::get(format!("http://{addr}/docs/swagger-ui.css"))
            .await?
            .error_for_status()?;
        assert!(
            css.headers()[reqwest::header::CONTENT_TYPE]
                .to_str()?
                .contains("css")
        );

        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_json_metrics_endpoints() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;

        let summary_response = reqwest::get(format!("http://{addr}/v1/metrics/summary"))
            .await?
            .error_for_status()?;
        assert!(summary_response.headers().get("x-request-id").is_some());
        let summary: serde_json::Value = summary_response.json().await?;
        assert!(summary["uptime_seconds"].is_number());
        assert!(summary["in_flight"].is_number());
        assert!(summary["dhcpv4"]["messages_received"].is_number());
        assert!(summary["dhcpv6"]["messages_sent"].is_number());

        let detailed_response = reqwest::get(format!("http://{addr}/v1/metrics"))
            .await?
            .error_for_status()?;
        assert!(detailed_response.headers().get("x-request-id").is_some());
        let detailed: serde_json::Value = detailed_response.json().await?;
        assert!(detailed["summary"].is_object());
        assert!(detailed["counters"].is_object());
        assert!(detailed["gauges"].is_object());
        assert!(detailed["histograms"].is_object());

        let prometheus_response = reqwest::get(format!("http://{addr}/v1/metrics/prometheus"))
            .await?
            .error_for_status()?;
        assert!(prometheus_response.headers().get("x-request-id").is_some());
        let prometheus: serde_json::Value = prometheus_response.json().await?;
        assert!(
            prometheus["families"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
        );
        assert!(
            prometheus["families"]
                .as_array()
                .unwrap()
                .iter()
                .any(|family| family["name"] == "uptime")
        );

        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_server_info() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::get(format!("http://{addr}/v1/server"))
            .await?
            .error_for_status()?;
        let header_request_id = response
            .headers()
            .get("x-request-id")
            .expect("x-request-id header")
            .to_str()?
            .to_string();
        let body: serde_json::Value = response.json().await?;

        assert_eq!(body["id"], "dora_id");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["mode"], "normal");
        assert_eq!(body["api"]["version"], "v1");
        assert!(body["api"]["auth"].as_array().is_some());
        assert!(body["started_at"].as_str().is_some_and(|v| !v.is_empty()));
        assert_eq!(body["request_id"], header_request_id);

        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_config_returns_redacted_json() -> anyhow::Result<()> {
        let cfg_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../example.yaml")
            .canonicalize()?;
        let cfg = Arc::new(DhcpConfig::parse(&cfg_path)?);
        let (addr, token) =
            spawn_test_api_with_config(Health::Good, ApiAuth::disabled(), cfg).await?;

        let response = reqwest::get(format!("http://{addr}/v1/config"))
            .await?
            .error_for_status()?;
        let header_request_id = response
            .headers()
            .get("x-request-id")
            .expect("x-request-id header")
            .to_str()?
            .to_string();
        assert!(
            !header_request_id.is_empty(),
            "x-request-id must be non-empty"
        );
        let body: serde_json::Value = response.json().await?;

        // no candidate has been activated in this test, so the active version is
        // the bootstrap (on-disk) config
        assert_eq!(body["version"], "bootstrap");
        assert_eq!(body["redacted"], true);
        assert!(body["document"].is_object());
        assert!(body.to_string().contains("**REDACTED**"));

        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_split_lease_endpoints() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;

        let v4_response = reqwest::get(format!(
            "http://{addr}/v1/leases/v4?limit=25&offset=0&sort=state,-expires_at,ip"
        ))
        .await?
        .error_for_status()?;
        let v4_body: serde_json::Value = v4_response.json().await?;
        assert_eq!(v4_body["meta"]["limit"], 25);
        assert_eq!(v4_body["meta"]["offset"], 0);
        assert_eq!(v4_body["meta"]["count"], 0);
        assert_eq!(v4_body["meta"]["total"], 0);
        assert_eq!(v4_body["meta"]["sort"][0], "state");
        assert!(
            v4_body["items"]
                .as_array()
                .is_some_and(|items| items.is_empty())
        );

        let v6_response = reqwest::get(format!("http://{addr}/v1/leases/v6"))
            .await?
            .error_for_status()?;
        let v6_body: serde_json::Value = v6_response.json().await?;
        assert_eq!(v6_body["meta"]["limit"], 100);
        assert_eq!(v6_body["meta"]["offset"], 0);
        assert!(
            v6_body["items"]
                .as_array()
                .is_some_and(|items| items.is_empty())
        );

        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn test_bearer_auth_protects_v1_routes_when_configured() -> anyhow::Result<()> {
        let (addr, token) =
            spawn_test_api_with_auth(Health::Good, ApiAuth::bearer("secret")).await?;

        let missing = reqwest::get(format!("http://{addr}/v1/server"))
            .await?
            .error_for_status()
            .expect_err("missing bearer token should be unauthorized");
        assert_eq!(missing.status(), Some(reqwest::StatusCode::UNAUTHORIZED));

        let bad = reqwest::Client::new()
            .get(format!("http://{addr}/v1/server"))
            .bearer_auth("wrong")
            .send()
            .await?
            .error_for_status()
            .expect_err("invalid bearer token should be unauthorized");
        assert_eq!(bad.status(), Some(reqwest::StatusCode::UNAUTHORIZED));

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/v1/server"))
            .bearer_auth("secret")
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["api"]["auth"][0], "bearer");

        let leases = reqwest::Client::new()
            .get(format!("http://{addr}/v1/leases/v4"))
            .bearer_auth("secret")
            .send()
            .await?
            .error_for_status()?;
        let leases_body: serde_json::Value = leases.json().await?;
        assert!(leases_body["items"].as_array().is_some());

        let public = reqwest::get(format!("http://{addr}/health"))
            .await?
            .error_for_status()?;
        let public_body: serde_json::Value = public.json().await?;
        assert_eq!(public_body["status"], "alive");

        token.cancel();
        Ok(())
    }

    // lease query filters and multi-field sort are actually applied (not just
    // echoed into meta).
    #[test]
    fn test_lease_filter_and_sort() {
        use crate::handlers::{LeaseListQuery, filter_and_sort};
        use crate::models::V4Lease;

        let mk = |ip: &str, state: &str, cid: &str, exp: &str| V4Lease {
            family: "v4".to_string(),
            state: state.to_string(),
            ip: ip.to_string(),
            network: "10.0.0.0/24".to_string(),
            client_id: Some(cid.to_string()),
            expires_at: Some(exp.to_string()),
            source: Some("database".to_string()),
        };
        let items = vec![
            mk("10.0.0.3", "leased", "aa", "2030-01-01T00:00:00+00:00"),
            mk("10.0.0.1", "probated", "bb", "2020-01-01T00:00:00+00:00"),
            mk("10.0.0.2", "leased", "cc", "2025-01-01T00:00:00+00:00"),
        ];

        // filter: only leased
        let q = LeaseListQuery {
            state: Some("leased".to_string()),
            ..Default::default()
        };
        let out = filter_and_sort(items.clone(), &q).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|l| l.state == "leased"));

        // sort: descending by expiry
        let q = LeaseListQuery {
            sort: Some("-expires_at".to_string()),
            ..Default::default()
        };
        let out = filter_and_sort(items.clone(), &q).unwrap();
        assert_eq!(out.first().unwrap().ip, "10.0.0.3");
        assert_eq!(out.last().unwrap().ip, "10.0.0.1");

        // filter: time window excludes the 2020 and 2030 rows
        let q = LeaseListQuery {
            expires_from: Some("2024-01-01T00:00:00Z".to_string()),
            expires_to: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let out = filter_and_sort(items.clone(), &q).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ip, "10.0.0.2");

        // a malformed date is a 400, not silently ignored
        let q = LeaseListQuery {
            expires_from: Some("not-a-date".to_string()),
            ..Default::default()
        };
        assert!(filter_and_sort(items, &q).is_err());
    }

    // an error response uses the standard envelope { error: { code, message,
    // request_id } } and carries an X-Request-ID header.
    #[tokio::test]
    async fn test_error_envelope_shape() -> anyhow::Result<()> {
        let (addr, token) =
            spawn_test_api_with_auth(Health::Good, ApiAuth::bearer("secret")).await?;

        // no bearer token -> 401 with the structured error body
        let response = reqwest::get(format!("http://{addr}/v1/server")).await?;
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        let header_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .expect("error response must carry X-Request-ID");

        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(body["error"]["message"].is_string());
        assert_eq!(body["error"]["request_id"], header_id);

        token.cancel();
        Ok(())
    }

    // /v1/reservations/v4 lists config reservations; /v6 is empty for now.
    #[tokio::test]
    async fn test_reservations_endpoints() -> anyhow::Result<()> {
        static RES_YAML: &str = r#"
v4:
  networks:
    192.168.5.0/24:
      ranges:
        - start: 192.168.5.2
          end: 192.168.5.250
          config: { lease_time: { default: 3600 } }
          options: { values: {} }
      reservations:
        - ip: 192.168.5.166
          match: { chaddr: f8:1a:67:1f:c9:7d }
          config: { lease_time: { default: 3600 } }
          options: { values: {} }
"#;
        let cfg = Arc::new(DhcpConfig::parse_str(RES_YAML)?);
        let (addr, token) =
            spawn_test_api_with_config(Health::Good, ApiAuth::disabled(), cfg).await?;

        let response = reqwest::get(format!("http://{addr}/v1/reservations/v4"))
            .await?
            .error_for_status()?;
        assert!(response.headers().get("x-request-id").is_some());
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["meta"]["total"], 1);
        let item = &body["items"][0];
        assert_eq!(item["family"], "v4");
        assert_eq!(item["ip"], "192.168.5.166");
        assert_eq!(item["source"], "config");
        assert_eq!(item["match"]["chaddr"], "f8:1a:67:1f:c9:7d");

        // client_id filter (hex chaddr)
        let filtered: serde_json::Value = reqwest::get(format!(
            "http://{addr}/v1/reservations/v4?client_id=f81a671fc97d"
        ))
        .await?
        .error_for_status()?
        .json()
        .await?;
        assert_eq!(filtered["meta"]["total"], 1);
        let none: serde_json::Value = reqwest::get(format!(
            "http://{addr}/v1/reservations/v4?client_id=deadbeef"
        ))
        .await?
        .error_for_status()?
        .json()
        .await?;
        assert_eq!(none["meta"]["total"], 0);

        // v6 reservations are empty today
        let v6: serde_json::Value = reqwest::get(format!("http://{addr}/v1/reservations/v6"))
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(v6["items"].as_array().unwrap().len(), 0);

        token.cancel();
        Ok(())
    }

    // the map_response middleware backfills X-Request-ID on responses whose
    // handler doesn't set it (e.g. the lease list).
    #[tokio::test]
    async fn test_request_id_backfilled_on_all_responses() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::get(format!("http://{addr}/v1/leases/v4"))
            .await?
            .error_for_status()?;
        assert!(
            response.headers().get("x-request-id").is_some(),
            "every response must carry X-Request-ID"
        );
        token.cancel();
        Ok(())
    }

    // very simple test for existence of metrics endpoint
    #[tokio::test]
    async fn test_metrics() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let bytes = reqwest::get(format!("http://{addr}/metrics"))
            .await?
            .error_for_status()?
            .bytes()
            .await;
        assert!(bytes.is_ok());
        token.cancel();

        Ok(())
    }

    // maintenance-mode toggles the shared mode, which /v1/server then reports.
    #[tokio::test]
    async fn test_maintenance_mode_sets_and_clears_mode() -> anyhow::Result<()> {
        let (addr, token, mode, _mgr) =
            spawn_test_api_full(Health::Good, ApiAuth::disabled()).await?;
        let client = reqwest::Client::new();

        // enter maintenance
        let body: serde_json::Value = client
            .post(format!("http://{addr}/v1/actions/maintenance-mode"))
            .json(&serde_json::json!({ "enabled": true, "reason": "patching" }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["action"], "maintenance-mode");
        assert_eq!(body["result"]["mode"], "maintenance");
        assert_eq!(mode.get(), ServerMode::Maintenance);

        let server: serde_json::Value = reqwest::get(format!("http://{addr}/v1/server"))
            .await?
            .json()
            .await?;
        assert_eq!(server["mode"], "maintenance");

        // leave maintenance
        client
            .post(format!("http://{addr}/v1/actions/maintenance-mode"))
            .json(&serde_json::json!({ "enabled": false }))
            .send()
            .await?
            .error_for_status()?;
        assert_eq!(mode.get(), ServerMode::Normal);

        token.cancel();
        Ok(())
    }

    // drain sets drain mode with an empty body (requestBody is optional).
    #[tokio::test]
    async fn test_drain_sets_mode_with_empty_body() -> anyhow::Result<()> {
        let (addr, token, mode, _mgr) =
            spawn_test_api_full(Health::Good, ApiAuth::disabled()).await?;

        let body: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/drain"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["result"]["mode"], "drain");
        assert_eq!(mode.get(), ServerMode::Drain);

        token.cancel();
        Ok(())
    }

    // shutdown returns 202 + an operation id, enters shutting-down mode, and
    // persists a retrievable operation record. Uses a long grace period so the
    // shared token isn't cancelled during the test.
    #[tokio::test]
    async fn test_shutdown_accepts_and_persists_operation() -> anyhow::Result<()> {
        let (addr, token, mode, _mgr) =
            spawn_test_api_full(Health::Good, ApiAuth::disabled()).await?;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/shutdown"))
            .json(&serde_json::json!({ "grace_period_seconds": 3600 }))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        let accepted: serde_json::Value = response.json().await?;
        assert_eq!(accepted["status"], "accepted");
        let operation_id = accepted["operation_id"]
            .as_str()
            .expect("operation_id")
            .to_string();
        assert_eq!(
            accepted["links"]["self"],
            format!("/v1/operations/{operation_id}")
        );
        assert_eq!(mode.get(), ServerMode::ShuttingDown);

        // the operation record is retrievable and describes the shutdown
        let op: serde_json::Value =
            reqwest::get(format!("http://{addr}/v1/operations/{operation_id}"))
                .await?
                .error_for_status()?
                .json()
                .await?;
        assert_eq!(op["operation_id"], operation_id);
        assert_eq!(op["action"], "shutdown");
        // accepted or running depending on the background task's progress
        assert!(matches!(
            op["status"].as_str(),
            Some("accepted") | Some("running")
        ));
        assert!(op["created_at"].is_string());

        token.cancel();
        Ok(())
    }

    // an unknown operation id yields a 404 in the standard error envelope.
    #[tokio::test]
    async fn test_get_operation_not_found() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::get(format!("http://{addr}/v1/operations/does-not-exist")).await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["error"]["code"], "not_found");
        assert!(body["error"]["request_id"].is_string());
        token.cancel();
        Ok(())
    }

    // a malformed / incomplete required body yields a 400 envelope, not axum's
    // default rejection.
    #[tokio::test]
    async fn test_maintenance_mode_rejects_bad_body() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/maintenance-mode"))
            .json(&serde_json::json!({ "reason": "missing enabled" }))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["error"]["code"], "bad_request");
        token.cancel();
        Ok(())
    }

    // action endpoints are gated by the bearer token when one is configured.
    #[tokio::test]
    async fn test_actions_require_auth_when_configured() -> anyhow::Result<()> {
        let (addr, token, _mode, _mgr) =
            spawn_test_api_full(Health::Good, ApiAuth::bearer("secret")).await?;

        let unauthorized = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/drain"))
            .send()
            .await?;
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        let ok = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/drain"))
            .bearer_auth("secret")
            .send()
            .await?;
        assert_eq!(ok.status(), reqwest::StatusCode::OK);

        token.cancel();
        Ok(())
    }

    // once shutdown has begun, mode-changing actions are 409 Conflict so nothing
    // can re-enable serving while the grace period counts down.
    #[tokio::test]
    async fn test_actions_rejected_after_shutdown() -> anyhow::Result<()> {
        let (addr, token, _mode, _mgr) =
            spawn_test_api_full(Health::Good, ApiAuth::disabled()).await?;
        // long grace so the shared token isn't cancelled during the test
        let accepted = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/shutdown"))
            .json(&serde_json::json!({ "grace_period_seconds": 3600 }))
            .send()
            .await?;
        assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);

        let drain = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/drain"))
            .send()
            .await?;
        assert_eq!(drain.status(), reqwest::StatusCode::CONFLICT);
        let body: serde_json::Value = drain.json().await?;
        assert_eq!(body["error"]["code"], "conflict");

        let maintenance = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/maintenance-mode"))
            .json(&serde_json::json!({ "enabled": false }))
            .send()
            .await?;
        assert_eq!(maintenance.status(), reqwest::StatusCode::CONFLICT);

        token.cancel();
        Ok(())
    }

    // create a v4 runtime reservation, then see it in the listing as `runtime`.
    #[tokio::test]
    async fn test_create_and_list_v4_reservation() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();

        let created = client
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&serde_json::json!({
                "family": "v4",
                "reservation": {
                    "ip": "192.168.9.9",
                    "match": { "chaddr": "01:02:03:04:05:06" }
                }
            }))
            .send()
            .await?;
        assert_eq!(created.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = created.json().await?;
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["action"], "create-reservation");

        let list: serde_json::Value =
            reqwest::get(format!("http://{addr}/v1/reservations/v4?ip=192.168.9.9"))
                .await?
                .json()
                .await?;
        assert_eq!(list["items"][0]["ip"], "192.168.9.9");
        assert_eq!(list["items"][0]["source"], "runtime");
        assert_eq!(list["items"][0]["match"]["chaddr"], "01:02:03:04:05:06");

        token.cancel();
        Ok(())
    }

    // creating the same address twice is a 409 conflict.
    #[tokio::test]
    async fn test_create_reservation_conflict() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "family": "v4",
            "reservation": { "ip": "192.168.9.10", "match": { "chaddr": "01:02:03:04:05:07" } }
        });
        let first = client
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&body)
            .send()
            .await?;
        assert_eq!(first.status(), reqwest::StatusCode::OK);
        let second = client
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&body)
            .send()
            .await?;
        assert_eq!(second.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(
            second.json::<serde_json::Value>().await?["error"]["code"],
            "conflict"
        );

        token.cancel();
        Ok(())
    }

    // delete removes a reservation; deleting a missing one is 404.
    #[tokio::test]
    async fn test_delete_reservation() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();
        client
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&serde_json::json!({
                "family": "v4",
                "reservation": { "ip": "192.168.9.11", "match": { "chaddr": "01:02:03:04:05:08" } }
            }))
            .send()
            .await?
            .error_for_status()?;

        let deleted = client
            .post(format!("http://{addr}/v1/actions/delete-reservation"))
            .json(&serde_json::json!({ "family": "v4", "ip": "192.168.9.11" }))
            .send()
            .await?;
        assert_eq!(deleted.status(), reqwest::StatusCode::OK);

        // gone from the listing
        let list: serde_json::Value =
            reqwest::get(format!("http://{addr}/v1/reservations/v4?ip=192.168.9.11"))
                .await?
                .json()
                .await?;
        assert_eq!(list["items"].as_array().map(|a| a.len()), Some(0));

        // deleting again is 404
        let missing = client
            .post(format!("http://{addr}/v1/actions/delete-reservation"))
            .json(&serde_json::json!({ "family": "v4", "ip": "192.168.9.11" }))
            .send()
            .await?;
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

        token.cancel();
        Ok(())
    }

    // v6 reservation round-trips through the write API and the v6 listing.
    #[tokio::test]
    async fn test_create_and_list_v6_reservation() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();
        let created = client
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&serde_json::json!({
                "family": "v6",
                "reservation": {
                    "ip": "2001:db8:1::120",
                    "match": { "duid": "0001000112ab" }
                }
            }))
            .send()
            .await?;
        assert_eq!(created.status(), reqwest::StatusCode::OK);

        let list: serde_json::Value = reqwest::get(format!("http://{addr}/v1/reservations/v6"))
            .await?
            .json()
            .await?;
        assert_eq!(list["items"][0]["source"], "runtime");
        assert_eq!(list["items"][0]["match"]["duid"], "0001000112ab");

        token.cancel();
        Ok(())
    }

    // async:true yields a 202 with an operation id.
    #[tokio::test]
    async fn test_create_reservation_async_returns_202() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&serde_json::json!({
                "family": "v4",
                "reservation": { "ip": "192.168.9.12", "match": { "chaddr": "01:02:03:04:05:09" } },
                "async": true
            }))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["status"], "accepted");
        assert!(body["operation_id"].as_str().is_some());

        token.cancel();
        Ok(())
    }

    // a malformed reservation body is a 400 envelope.
    #[tokio::test]
    async fn test_create_reservation_bad_body() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/create-reservation"))
            .json(&serde_json::json!({
                "family": "v4",
                "reservation": { "ip": "192.168.9.13" } // missing match
            }))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<serde_json::Value>().await?["error"]["code"],
            "bad_request"
        );

        token.cancel();
        Ok(())
    }

    // updating a reservation that doesn't exist is a 404 (not a silent create).
    #[tokio::test]
    async fn test_update_nonexistent_reservation_is_404() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/actions/update-reservation"))
            .json(&serde_json::json!({
                "family": "v4",
                "reservation": { "ip": "192.168.9.99", "match": { "chaddr": "aa:bb:cc:dd:ee:01" } }
            }))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            response.json::<serde_json::Value>().await?["error"]["code"],
            "not_found"
        );

        token.cancel();
        Ok(())
    }

    // ---- TLS / mTLS -------------------------------------------------------

    struct TestPki {
        server_cert: String,
        server_key: String,
        ca_cert: String,
        /// client cert + key concatenated (a reqwest Identity PEM)
        client_identity: String,
    }

    /// Generate a self-signed server cert (with a 127.0.0.1 IP SAN), a CA, and a
    /// client cert signed by that CA.
    fn gen_pki() -> TestPki {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};

        let mut server_params = CertificateParams::new(vec![]).unwrap();
        server_params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
        let server_key = KeyPair::generate().unwrap();
        let server_cert = server_params.self_signed(&server_key).unwrap();

        let mut ca_params = CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let client_params = CertificateParams::new(vec![]).unwrap();
        let client_key = KeyPair::generate().unwrap();
        // rcgen 0.14: signing now goes through an owned `Issuer` (params + key)
        let ca_issuer = Issuer::new(ca_params, ca_key);
        let client_cert = client_params.signed_by(&client_key, &ca_issuer).unwrap();

        TestPki {
            server_cert: server_cert.pem(),
            server_key: server_key.serialize_pem(),
            ca_cert: ca_cert.pem(),
            client_identity: format!("{}{}", client_cert.pem(), client_key.serialize_pem()),
        }
    }

    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    /// Write the server cert/key (and optionally the client CA) to a temp dir and
    /// return a `TlsConfig` pointing at them.
    fn write_tls_files(
        pki: &TestPki,
        with_client_ca: bool,
        require_client_auth: bool,
    ) -> crate::tls::TlsConfig {
        let dir =
            std::env::temp_dir().join(format!("dora-tls-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        std::fs::write(&cert, &pki.server_cert).unwrap();
        let key = dir.join("key.pem");
        std::fs::write(&key, &pki.server_key).unwrap();
        let client_ca = with_client_ca.then(|| {
            let ca = dir.join("ca.pem");
            std::fs::write(&ca, &pki.ca_cert).unwrap();
            ca
        });
        crate::tls::TlsConfig {
            cert,
            key,
            client_ca,
            require_client_auth,
            reload_interval: Duration::from_secs(60),
        }
    }

    /// Spawn the API over TLS on an ephemeral port using `tls::serve` directly.
    async fn spawn_tls_api(
        mut auth: ApiAuth,
        tls: crate::tls::TlsConfig,
    ) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        // mirror ExternalApi::with_tls: advertise mtls when a client-CA is set
        auth.mtls_enabled = tls.client_ca.is_some();
        let mgr = Arc::new(IpManager::new(PostgresDb::new_test().await?)?);
        let cfg = Arc::new(DhcpConfig::default());
        let state = models::blank_health();
        *state.lock() = Health::Good;
        let token = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = api_router::<PostgresDb>(
            state,
            ApiState {
                started_at: SystemTime::now(),
            },
            auth,
            cfg,
            mgr,
            SharedMode::default(),
            RuntimeReservations::new(),
            token.clone(),
            Duration::from_secs(30),
        );
        let tls_state = crate::tls::TlsState::load(tls)?;
        let sd = token.clone();
        tokio::spawn(async move {
            if let Err(err) = crate::tls::serve(listener, tls_state, app, sd).await {
                tracing::error!(?err, "test TLS API returned error");
            }
        });
        Ok((addr, token))
    }

    // TLS termination: the API is reachable over HTTPS with the server cert.
    #[tokio::test]
    async fn test_tls_serves_https() -> anyhow::Result<()> {
        let pki = gen_pki();
        let (addr, token) =
            spawn_tls_api(ApiAuth::disabled(), write_tls_files(&pki, false, false)).await?;
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(pki.server_cert.as_bytes())?)
            .build()?;
        let body: serde_json::Value = client
            .get(format!("https://{addr}/health"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(body["status"], "alive");
        token.cancel();
        Ok(())
    }

    // A verified client certificate authenticates a request even when a bearer
    // token is configured and none is sent (mTLS OR bearer).
    #[tokio::test]
    async fn test_mtls_client_cert_authenticates() -> anyhow::Result<()> {
        let pki = gen_pki();
        let (addr, token) = spawn_tls_api(
            ApiAuth::bearer("secret"),
            write_tls_files(&pki, true, false),
        )
        .await?;
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(pki.server_cert.as_bytes())?)
            .identity(reqwest::Identity::from_pem(pki.client_identity.as_bytes())?)
            .build()?;
        // protected endpoint, no Authorization header — mTLS carries it
        let resp = client
            .get(format!("https://{addr}/v1/server"))
            .send()
            .await?;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;
        // server advertises mtls in its auth methods
        let methods = body["api"]["auth"].as_array().unwrap();
        assert!(methods.iter().any(|m| m == "mtls"));
        token.cancel();
        Ok(())
    }

    // Without a client cert, a protected endpoint still requires the bearer token
    // (mTLS is optional), and a spoofed marker header is ignored.
    #[tokio::test]
    async fn test_no_client_cert_requires_bearer() -> anyhow::Result<()> {
        let pki = gen_pki();
        let (addr, token) = spawn_tls_api(
            ApiAuth::bearer("secret"),
            write_tls_files(&pki, true, false),
        )
        .await?;
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(pki.server_cert.as_bytes())?)
            .build()?;
        // no cert, no bearer, and a forged marker header -> still 401
        let unauth = client
            .get(format!("https://{addr}/v1/server"))
            .header(crate::MTLS_HEADER, "1")
            .send()
            .await?;
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
        // with the bearer token -> ok
        let ok = client
            .get(format!("https://{addr}/v1/server"))
            .bearer_auth("secret")
            .send()
            .await?;
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        token.cancel();
        Ok(())
    }

    // The spoof-protection also holds over plaintext: a client-supplied marker
    // header is stripped, so it can't bypass the bearer requirement.
    #[tokio::test]
    async fn test_spoofed_mtls_header_stripped_plaintext() -> anyhow::Result<()> {
        let (addr, token, _mode, _mgr) =
            spawn_test_api_full(Health::Good, ApiAuth::bearer("secret")).await?;
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/v1/server"))
            .header(crate::MTLS_HEADER, "1")
            .send()
            .await?;
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
        token.cancel();
        Ok(())
    }

    // Mandatory mTLS: a client-CA with no bearer requires a client cert at the
    // TLS layer, so a certless client can't even complete the handshake (it
    // can't reach the open API), while a valid client cert connects.
    #[tokio::test]
    async fn test_mandatory_mtls_requires_client_cert() -> anyhow::Result<()> {
        let pki = gen_pki();
        let (addr, token) =
            spawn_tls_api(ApiAuth::disabled(), write_tls_files(&pki, true, true)).await?;
        let root = reqwest::Certificate::from_pem(pki.server_cert.as_bytes())?;

        // no client cert -> handshake is rejected
        let certless = reqwest::Client::builder()
            .add_root_certificate(root.clone())
            .build()?;
        assert!(
            certless
                .get(format!("https://{addr}/health"))
                .send()
                .await
                .is_err()
        );

        // valid client cert -> connects and is authorized
        let client = reqwest::Client::builder()
            .add_root_certificate(root)
            .identity(reqwest::Identity::from_pem(pki.client_identity.as_bytes())?)
            .build()?;
        let resp = client
            .get(format!("https://{addr}/v1/server"))
            .send()
            .await?;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        token.cancel();
        Ok(())
    }

    // ---- config lifecycle -------------------------------------------------

    fn sample_config_document() -> serde_json::Value {
        yaml_serde::from_str(include_str!("../../libs/config/sample/config.yaml")).unwrap()
    }

    // stage a valid candidate -> it validates to `valid`.
    #[tokio::test]
    async fn test_stage_valid_config_candidate() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let staged: serde_json::Value = reqwest::Client::new()
            .put(format!("http://{addr}/v1/config"))
            .json(&serde_json::json!({ "document": sample_config_document(), "message": "test" }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(staged["status"], "accepted");
        let id = staged["operation_id"].as_str().unwrap();
        assert_eq!(
            staged["links"]["self"],
            format!("/v1/config/candidates/{id}")
        );

        let cand: serde_json::Value =
            reqwest::get(format!("http://{addr}/v1/config/candidates/{id}"))
                .await?
                .json()
                .await?;
        assert_eq!(cand["candidate_id"], id);
        assert_eq!(cand["status"], "valid");
        token.cancel();
        Ok(())
    }

    // an unparseable candidate validates to `invalid` with findings.
    #[tokio::test]
    async fn test_stage_invalid_config_candidate() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let staged: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{addr}/v1/config/candidates"))
            .json(&serde_json::json!({ "document": [1, 2, 3] }))
            .send()
            .await?
            .json()
            .await?;
        let id = staged["operation_id"].as_str().unwrap();
        let cand: serde_json::Value =
            reqwest::get(format!("http://{addr}/v1/config/candidates/{id}"))
                .await?
                .json()
                .await?;
        assert_eq!(cand["status"], "invalid");
        assert!(cand["validation"].as_array().is_some_and(|a| !a.is_empty()));
        token.cancel();
        Ok(())
    }

    // activate a candidate -> it becomes the active version reported by GET /v1/config.
    #[tokio::test]
    async fn test_activate_config_sets_active_version() -> anyhow::Result<()> {
        let dir =
            std::env::temp_dir().join(format!("dora-cfg-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("config.yaml");
        std::fs::write(&path, include_str!("../../libs/config/sample/config.yaml"))?;
        let cfg = Arc::new(DhcpConfig::parse(&path)?);
        let (addr, token) =
            spawn_test_api_with_config(Health::Good, ApiAuth::disabled(), cfg).await?;
        let client = reqwest::Client::new();

        let staged: serde_json::Value = client
            .put(format!("http://{addr}/v1/config"))
            .json(&serde_json::json!({ "document": sample_config_document() }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let id = staged["operation_id"].as_str().unwrap().to_string();

        let act = client
            .post(format!("http://{addr}/v1/actions/activate-config"))
            .json(&serde_json::json!({ "candidate_id": id }))
            .send()
            .await?;
        assert_eq!(act.status(), reqwest::StatusCode::ACCEPTED);

        let cfg_doc: serde_json::Value = client
            .get(format!("http://{addr}/v1/config"))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(cfg_doc["version"], id);

        token.cancel();
        Ok(())
    }

    // activating an unknown candidate is 404; an invalid one is 409.
    #[tokio::test]
    async fn test_activate_rejects_missing_and_invalid() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();

        let missing = client
            .post(format!("http://{addr}/v1/actions/activate-config"))
            .json(&serde_json::json!({ "candidate_id": "nope" }))
            .send()
            .await?;
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

        let staged: serde_json::Value = client
            .post(format!("http://{addr}/v1/config/candidates"))
            .json(&serde_json::json!({ "document": [1, 2, 3] }))
            .send()
            .await?
            .json()
            .await?;
        let id = staged["operation_id"].as_str().unwrap();
        let invalid = client
            .post(format!("http://{addr}/v1/actions/activate-config"))
            .json(&serde_json::json!({ "candidate_id": id }))
            .send()
            .await?;
        assert_eq!(invalid.status(), reqwest::StatusCode::CONFLICT);

        token.cancel();
        Ok(())
    }

    // when mTLS is enabled (client-CA configured), config writes require a client
    // cert (403 with only a bearer), while reads still accept the bearer.
    #[tokio::test]
    async fn test_config_write_requires_mtls_when_enabled() -> anyhow::Result<()> {
        let auth = ApiAuth {
            bearer_token: Some(std::sync::Arc::from("secret")),
            allow_unauthenticated: false,
            mtls_enabled: true,
        };
        let (addr, token, _mode, _mgr) = spawn_test_api_full(Health::Good, auth).await?;
        let client = reqwest::Client::new();

        let forbidden = client
            .put(format!("http://{addr}/v1/config"))
            .bearer_auth("secret")
            .json(&serde_json::json!({ "document": sample_config_document() }))
            .send()
            .await?;
        assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(
            forbidden.json::<serde_json::Value>().await?["error"]["code"],
            "forbidden"
        );

        let ok = client
            .get(format!("http://{addr}/v1/config/candidates"))
            .bearer_auth("secret")
            .send()
            .await?;
        assert_eq!(ok.status(), reqwest::StatusCode::OK);

        token.cancel();
        Ok(())
    }

    // ---- lease / DDNS actions ---------------------------------------------

    // releasing a lease that isn't present is a 404; a family/ip mismatch is 400.
    #[tokio::test]
    async fn test_release_lease_validation() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();

        let missing = client
            .post(format!("http://{addr}/v1/actions/release-lease"))
            .json(
                &serde_json::json!({ "family": "v4", "ip": "192.168.0.50", "client_id": "aabbcc" }),
            )
            .send()
            .await?;
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

        let mismatch = client
            .post(format!("http://{addr}/v1/actions/release-lease"))
            .json(&serde_json::json!({ "family": "v6", "ip": "192.168.0.50" }))
            .send()
            .await?;
        assert_eq!(mismatch.status(), reqwest::StatusCode::BAD_REQUEST);

        token.cancel();
        Ok(())
    }

    // trigger-ddns rejects a bad operation (400) and a v6 address (400), and
    // reports 409 when DDNS isn't configured.
    #[tokio::test]
    async fn test_trigger_ddns_validation() -> anyhow::Result<()> {
        let (addr, token) = spawn_test_api(Health::Good).await?;
        let client = reqwest::Client::new();

        let bad_op = client
            .post(format!("http://{addr}/v1/actions/trigger-ddns-update"))
            .json(&serde_json::json!({ "operation": "nope", "family": "v4", "ip": "192.168.0.50" }))
            .send()
            .await?;
        assert_eq!(bad_op.status(), reqwest::StatusCode::BAD_REQUEST);

        let v6 = client
            .post(format!("http://{addr}/v1/actions/trigger-ddns-update"))
            .json(
                &serde_json::json!({ "operation": "cleanup", "family": "v6", "ip": "2001:db8::1" }),
            )
            .send()
            .await?;
        assert_eq!(v6.status(), reqwest::StatusCode::BAD_REQUEST);

        // default test config has no DDNS -> 409
        let no_ddns = client
            .post(format!("http://{addr}/v1/actions/trigger-ddns-update"))
            .json(&serde_json::json!({ "operation": "cleanup", "family": "v4", "ip": "192.168.0.50" }))
            .send()
            .await?;
        assert_eq!(no_ddns.status(), reqwest::StatusCode::CONFLICT);

        token.cancel();
        Ok(())
    }
}
