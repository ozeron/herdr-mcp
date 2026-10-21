//! Builds the bridge and the HTTP front for `http-serve` and `tunnel ngrok run`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::caller::{CallerEnv, ProcessAncestry};
use crate::cli::ToolMode;
use crate::herdr::{SessionEnv, resolve_socket_path};
use crate::http::{self, HttpOptions, TokenCheck};
use crate::install::InstallEnv;
use crate::log::Logger;
use crate::oauth;
use crate::profile::{ClientKind, Profile};
use crate::server::{BridgeConfig, HerdrMcp};
use crate::tunnel::TunnelConfig;

/// A startup failure and the process exit code it maps to.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ServeError {
    pub message: String,
    pub code: u8,
}

impl ServeError {
    pub fn usage(err: impl std::fmt::Display) -> Self {
        Self {
            message: err.to_string(),
            code: 2,
        }
    }
}

pub struct BridgeOptions {
    pub session: Option<String>,
    pub client: Option<ClientKind>,
    pub max_wait_ms: Option<u64>,
    pub tools: ToolMode,
    pub tunnel: Option<TunnelConfig>,
}

pub fn build_bridge(options: BridgeOptions) -> Result<HerdrMcp, ServeError> {
    let socket_path = resolve_socket_path(options.session.as_deref(), &SessionEnv::from_process())
        .map_err(ServeError::usage)?;
    let logger = socket_path
        .parent()
        .map(|dir| dir.join("herdr-mcp"))
        .and_then(|dir| Logger::to_dir(dir).ok())
        .unwrap_or_else(Logger::stderr_only);
    let mut server = HerdrMcp::new(BridgeConfig {
        socket_path,
        profile: Profile::new(
            options.client,
            options.max_wait_ms.map(Duration::from_millis),
        ),
        mode: options.tools,
        ancestry: Arc::new(ProcessAncestry),
        caller_env: CallerEnv::from_process(),
        logger,
        install_env: InstallEnv::from_process().ok(),
    });
    if let Some(tunnel) = options.tunnel {
        server = server.with_tunnel(tunnel);
    }
    Ok(server)
}

pub struct HttpSpec {
    pub bind: SocketAddr,
    pub public_url: Option<String>,
    pub check: Arc<dyn TokenCheck>,
    /// `(audience, issuer)` for the protected-resource metadata, when OAuth is configured.
    pub metadata: Option<(String, String)>,
    pub bridge: BridgeOptions,
}

/// A bound listener and the router for it. Connections queue on the listener until
/// [`http::serve`] runs.
pub struct HttpServer {
    pub listener: TcpListener,
    pub router: Router,
    pub shutdown: CancellationToken,
    /// Tool calls running now, for draining on a signal.
    pub in_flight: Arc<AtomicUsize>,
}

pub async fn prepare_http(spec: HttpSpec) -> Result<HttpServer, ServeError> {
    let options =
        HttpOptions::new(spec.bind, spec.public_url.as_deref()).map_err(ServeError::usage)?;
    let listener = http::bind_loopback(spec.bind)
        .await
        .map_err(ServeError::usage)?;
    let server = build_bridge(spec.bridge)?;
    let in_flight = server.in_flight();
    let shutdown = CancellationToken::new();
    let mut router = http::mcp_router(server, &options, spec.check, shutdown.clone())
        .map_err(ServeError::usage)?;
    if let Some((audience, issuer)) = &spec.metadata {
        router = router.merge(oauth::metadata_router(audience, issuer));
    }
    Ok(HttpServer {
        listener,
        router,
        shutdown,
        in_flight,
    })
}
