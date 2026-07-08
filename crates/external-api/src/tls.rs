//! In-process TLS + optional mTLS termination for the management API.
//!
//! dora terminates TLS itself (Cilium passes TLS through). The server
//! certificate (from an external ACME client) and the client-certificate trust
//! anchors (from an external TAMP client) arrive as PEM files on disk; a
//! background task polls them and hot-swaps the whole rustls `ServerConfig` when
//! they rotate, so renewals take effect without a restart. mTLS is optional at
//! the transport layer — a client may present a certificate or not — and
//! [`crate`]'s `authorize` accepts either a valid client certificate or a Bearer
//! token.

use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::RwLock;
use tokio::net::TcpListener;
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        RootCertStore, ServerConfig,
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer},
        server::WebPkiClientVerifier,
    },
};
use tracing::{debug, info, warn};

/// Operational TLS settings: paths to the PEM files delivered by the external
/// ACME (server cert/key) and TAMP (client-cert trust anchors) clients.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// server certificate chain (PEM)
    pub cert: PathBuf,
    /// server private key (PEM)
    pub key: PathBuf,
    /// trust anchors for verifying client certificates (PEM). `None` disables
    /// mTLS; a bundle enables *optional* mTLS (a presented cert is verified, but
    /// a client may still connect without one and authenticate by Bearer token).
    pub client_ca: Option<PathBuf>,
    /// how often to re-read the files and hot-swap on rotation
    pub reload_interval: Duration,
}

/// The raw bytes last loaded from the TLS files, used to detect rotation cheaply
/// (the files are small, so re-reading and comparing is fine).
#[derive(PartialEq, Eq)]
struct Fingerprint {
    cert: Vec<u8>,
    key: Vec<u8>,
    client_ca: Option<Vec<u8>>,
}

/// Shared TLS state: the current `ServerConfig` (swapped atomically on reload)
/// plus what's needed to rebuild it.
pub struct TlsState {
    config: RwLock<Arc<ServerConfig>>,
    fingerprint: RwLock<Fingerprint>,
    tls: TlsConfig,
    provider: Arc<CryptoProvider>,
}

impl fmt::Debug for TlsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never print the fingerprint — it holds raw private-key bytes
        f.debug_struct("TlsState")
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl TlsState {
    /// Load the TLS files and build the initial server config.
    pub fn load(tls: TlsConfig) -> Result<Arc<Self>> {
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let (config, fingerprint) = build_config(&tls, &provider)?;
        info!(
            cert = %tls.cert.display(),
            mtls = tls.client_ca.is_some(),
            "management API TLS enabled"
        );
        Ok(Arc::new(Self {
            config: RwLock::new(config),
            fingerprint: RwLock::new(fingerprint),
            tls,
            provider,
        }))
    }

    /// The current server config (cheap Arc clone).
    fn current(&self) -> Arc<ServerConfig> {
        self.config.read().clone()
    }

    /// Re-read the files; if any changed, rebuild and swap the config. Returns
    /// whether a reload happened. Errors leave the current config in place.
    fn reload_if_changed(&self) -> Result<bool> {
        let fingerprint = read_fingerprint(&self.tls)?;
        if *self.fingerprint.read() == fingerprint {
            return Ok(false);
        }
        let (config, fingerprint) = build_config(&self.tls, &self.provider)?;
        *self.config.write() = config;
        *self.fingerprint.write() = fingerprint;
        Ok(true)
    }

    /// The reload interval.
    fn interval(&self) -> Duration {
        self.tls.reload_interval
    }
}

/// Poll the TLS files periodically and hot-swap the config on rotation. Runs
/// until `shutdown` fires. Spawn under the API's shared cancellation token.
pub async fn reload_task(state: Arc<TlsState>, shutdown: tokio_util::sync::CancellationToken) {
    let interval = state.interval();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => match state.reload_if_changed() {
                Ok(true) => info!("reloaded management API TLS certificates"),
                Ok(false) => {}
                Err(err) => warn!(?err, "failed to reload TLS certificates; keeping current"),
            },
        }
    }
}

/// Per-connection info injected into each request's extensions: the peer
/// address and whether the client authenticated with a trusted certificate
/// (mTLS). Handlers reach it via the `ClientAuth` middleware.
#[derive(Clone, Debug)]
pub struct ConnData {
    /// remote peer address
    pub addr: SocketAddr,
    /// a trusted client certificate was presented and verified at the TLS layer
    pub mtls_authenticated: bool,
}

/// Serve the axum `Router` over TLS on `tcp` until `shutdown` fires.
///
/// axum 0.7's `serve` only drives a plain `TcpListener`, so we run the accept
/// loop ourselves: for each connection we perform the rustls handshake with the
/// current (hot-reloadable) config, note whether a verified client certificate
/// was presented, inject that [`ConnData`] into the request extensions, and hand
/// the connection to hyper. On shutdown we stop accepting and let in-flight
/// connections drain gracefully.
pub async fn serve(
    tcp: TcpListener,
    state: Arc<TlsState>,
    app: axum::Router,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;

    loop {
        let (stream, addr) = tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = tcp.accept() => match accepted {
                Ok(conn) => conn,
                Err(err) => {
                    debug!(?err, "tcp accept failed");
                    continue;
                }
            },
        };

        let acceptor = TlsAcceptor::from(state.current());
        let app = app.clone();
        let conn_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(err) => {
                    debug!(?err, %addr, "tls handshake failed");
                    return;
                }
            };
            let mtls_authenticated = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .is_some_and(|certs| !certs.is_empty());
            let conn_data = ConnData {
                addr,
                mtls_authenticated,
            };

            // per-connection service: stamp ConnData onto every request, then
            // run it through the shared Router (cloned per request via oneshot)
            let hyper_service = hyper::service::service_fn(
                move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(conn_data.clone());
                    let app = app.clone();
                    async move {
                        use tower::ServiceExt;
                        app.oneshot(req).await
                    }
                },
            );

            let builder = Builder::new(TokioExecutor::new());
            let conn =
                builder.serve_connection_with_upgrades(TokioIo::new(tls_stream), hyper_service);
            tokio::pin!(conn);
            tokio::select! {
                res = conn.as_mut() => {
                    if let Err(err) = res {
                        debug!(?err, %addr, "error serving TLS connection");
                    }
                }
                _ = conn_shutdown.cancelled() => {
                    // begin a graceful shutdown of this connection, then finish
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
        });
    }
    Ok(())
}

/// Build a `ServerConfig` from the current files, returning it with the
/// fingerprint of the bytes it was built from.
fn build_config(
    tls: &TlsConfig,
    provider: &Arc<CryptoProvider>,
) -> Result<(Arc<ServerConfig>, Fingerprint)> {
    let fingerprint = read_fingerprint(tls)?;
    let certs = load_certs(&fingerprint.cert)
        .with_context(|| format!("loading server certificate {}", tls.cert.display()))?;
    let key = load_key(&fingerprint.key)
        .with_context(|| format!("loading server key {}", tls.key.display()))?;

    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("no supported TLS protocol versions")?;

    let config = match &fingerprint.client_ca {
        Some(ca_bytes) => {
            let mut roots = RootCertStore::empty();
            for cert in load_certs(ca_bytes).with_context(|| {
                format!(
                    "loading client-cert trust anchors {}",
                    tls.client_ca.as_ref().unwrap().display()
                )
            })? {
                roots.add(cert).context("adding client-cert trust anchor")?;
            }
            // optional mTLS: verify a presented client cert, but allow clients
            // that present none (they must authenticate by Bearer token).
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                    .allow_unauthenticated()
                    .build()
                    .context("building client-cert verifier")?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .context("invalid server certificate/key")?
        }
        None => builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("invalid server certificate/key")?,
    };

    Ok((Arc::new(config), fingerprint))
}

/// Read the current bytes of the TLS files (the change-detection fingerprint).
fn read_fingerprint(tls: &TlsConfig) -> Result<Fingerprint> {
    Ok(Fingerprint {
        cert: read_file(&tls.cert)?,
        key: read_file(&tls.key)?,
        client_ca: tls.client_ca.as_deref().map(read_file).transpose()?,
    })
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

fn load_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let certs = rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing PEM certificates")?;
    if certs.is_empty() {
        bail!("no certificates found in PEM");
    }
    Ok(certs)
}

fn load_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut &pem[..])
        .context("parsing PEM private key")?
        .ok_or_else(|| anyhow!("no private key found in PEM"))
}
