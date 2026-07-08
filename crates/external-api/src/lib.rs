//! # Healthcheck & API
//!
//! This crate provides http api's for healthcheck, diagnostics, and metrics
//! It exposes the following endpoints:
//!
//! /health
//! /ping
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
use axum::{Router, extract::Extension, routing};

use ip_manager::{IpManager, Storage};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace};

pub use crate::models::{Health, State};
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
}

#[derive(Debug, Clone)]
struct ApiState {
    started_at: SystemTime,
}

#[derive(Debug, Clone)]
struct ApiAuth {
    bearer_token: Option<Arc<str>>,
}

impl ApiAuth {
    fn from_env() -> Self {
        let bearer_token = std::env::var("DORA_API_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .map(Arc::<str>::from);

        Self { bearer_token }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self { bearer_token: None }
    }

    #[cfg(test)]
    fn bearer(token: &str) -> Self {
        Self {
            bearer_token: Some(Arc::<str>::from(token)),
        }
    }

    fn auth_methods(&self) -> Vec<String> {
        if self.bearer_token.is_some() {
            vec!["bearer".to_string()]
        } else {
            Vec::new()
        }
    }
}

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
        }
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
        token: CancellationToken,
    ) -> Result<()> {
        const TIMEOUT: u64 = 30;
        let service = api_router::<S>(
            state,
            api_state,
            auth,
            cfg,
            ip_mgr,
            Duration::from_secs(TIMEOUT),
        );

        let tcp = TcpListener::bind(&addr).await?;
        tracing::debug!(%addr, "external API listening");

        axum::serve(tcp, service)
            .with_graceful_shutdown(async move {
                token.cancelled().await;
            })
            .await?;
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
        // if tx is not cloned, health listen will never update since ExternalApi is owner

        tokio::spawn(async move {
            // `run` will exit when cancel token completes
            tokio::select! {
                r = ExternalApi::run(addr, state, api_state, auth, cfg, ip_mgr, token) => {
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
    timeout: Duration,
) -> Router {
    use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

    Router::new()
        .route("/health", routing::get(handlers::health))
        .route("/ready", routing::get(handlers::ready))
        .route("/openapi.json", routing::get(handlers::openapi_json))
        .route("/v1/server", routing::get(handlers::server_info))
        .route("/ping", routing::get(handlers::ping))
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
        .route("/v1/config", routing::get(handlers::config))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .layer(Extension(state))
        .layer(Extension(api_state))
        .layer(Extension(auth))
        .layer(Extension(ip_mgr))
        .layer(Extension(cfg))
}

mod handlers {

    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use anyhow::Context;
    use axum::{
        body::Body,
        extract::{Extension, Query},
        http::header,
        http::{HeaderMap, HeaderValue, Response, StatusCode, header::AUTHORIZATION},
        response::IntoResponse,
    };
    use chrono::{DateTime, Utc};
    use config::DhcpConfig;
    use dora_core::metrics::{START_TIME, UPTIME};
    use ip_manager::{IpManager, Storage};
    use ipnet::Ipv4Net;
    use prometheus::{Encoder, ProtobufEncoder, TextEncoder};
    use serde::Deserialize;
    use tracing::{error, warn};

    use crate::models::{
        ConfigDocument, Health, HealthResponse, Histogram, HistogramBucket, MetricFamily,
        MetricSample, MetricsDetailed, MetricsSummary, OpenMetricsJson, PaginationMeta,
        ProtocolMetricsSummary, ReadinessCheck, ReadinessResponse, ReadinessStatus, ReserveIp,
        ServerApiInfo, ServerInfo, ServerMode, ServerResult, State, V4Lease, V4LeaseListResponse,
        V6Lease, V6LeaseListResponse,
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
                mode: ServerMode::Normal,
                api: ServerApiInfo {
                    version: "v1".to_string(),
                    auth: auth_methods,
                },
                request_id,
            }),
        ))
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

    fn authorize(headers: &HeaderMap, auth: &crate::ApiAuth) -> ServerResult<()> {
        let Some(expected) = auth.bearer_token.as_deref() else {
            return Ok(());
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

        if actual == expected {
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
        items.sort_by(|a, b| a.ip.cmp(&b.ip));

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
        let mut items = ip_mgr
            .select_all()
            .await?
            .into_iter()
            .filter_map(|lease| v6_lease_from_state(&cfg, lease).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        items.sort_by(|a, b| a.ip.cmp(&b.ip));

        let (meta, items) = paginate(items, &query);
        Ok(axum::Json(V6LeaseListResponse { meta, items }))
    }

    #[derive(Debug, Default, Deserialize)]
    pub(crate) struct LeaseListQuery {
        limit: Option<usize>,
        offset: Option<usize>,
        state: Option<String>,
        network: Option<String>,
        ip: Option<String>,
        client_id: Option<String>,
        expires_from: Option<String>,
        expires_to: Option<String>,
        sort: Option<String>,
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

    pub(crate) async fn config(
        headers: HeaderMap,
        Extension(auth): Extension<crate::ApiAuth>,
        Extension(cfg): Extension<Arc<DhcpConfig>>,
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
        Ok((
            request_id_header(&request_id)?,
            axum::Json(ConfigDocument {
                version: "v1".to_string(),
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

    pub(crate) async fn metrics() -> ServerResult<impl IntoResponse> {
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

    pub(crate) async fn metrics_text() -> ServerResult<impl IntoResponse> {
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

    pub(crate) async fn ping() -> impl IntoResponse {
        StatusCode::OK
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

    /// Server mode.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Copy, Clone, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ServerMode {
        /// Normal serving mode.
        Normal,
        /// Maintenance mode.
        Maintenance,
        /// Drain mode.
        Drain,
        /// Shutting down.
        ShuttingDown,
    }

    /// API metadata.
    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Eq)]
    pub struct ServerApiInfo {
        /// API version.
        pub version: String,
        /// Enabled authentication mechanisms.
        pub auth: Vec<String>,
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

    use ip_manager::sqlite::SqliteDb;
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn spawn_test_api(health: Health) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        spawn_test_api_with_auth(health, ApiAuth::disabled()).await
    }

    async fn spawn_test_api_with_auth(
        health: Health,
        auth: ApiAuth,
    ) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        let mgr = Arc::new(IpManager::new(SqliteDb::new("sqlite::memory:").await?)?);
        let cfg = Arc::new(DhcpConfig::default());
        let state = models::blank_health();
        *state.lock() = health;
        let token = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = api_router::<SqliteDb>(
            state,
            ApiState {
                started_at: SystemTime::now(),
            },
            auth,
            cfg,
            mgr,
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

    async fn spawn_test_api_with_config(
        health: Health,
        auth: ApiAuth,
        cfg: Arc<DhcpConfig>,
    ) -> anyhow::Result<(SocketAddr, CancellationToken)> {
        let mgr = Arc::new(IpManager::new(SqliteDb::new("sqlite::memory:").await?)?);
        let state = models::blank_health();
        *state.lock() = health;
        let token = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = api_router::<SqliteDb>(
            state,
            ApiState {
                started_at: SystemTime::now(),
            },
            auth,
            cfg,
            mgr,
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

        assert_eq!(body["version"], "v1");
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
}
