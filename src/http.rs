//! MCP over Streamable HTTP on localhost, behind an auth hook.
//!
//! Every request to `/mcp` goes through a [`TokenCheck`] first. With no check configured the
//! server uses [`DenyAll`], so it fails closed.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::server::HerdrMcp;

/// The MCP endpoint path.
pub const MCP_PATH: &str = "/mcp";

/// A request the check turned down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied;

/// Decides whether a bearer token may reach `/mcp`. `bearer` is the token after `Bearer `, or
/// `None` when the header is missing or is not a bearer credential.
pub trait TokenCheck: Send + Sync + 'static {
    fn check(&self, bearer: Option<&str>) -> Result<(), Denied>;
}

/// Refuses every request. The default until real auth is configured.
pub struct DenyAll;

impl TokenCheck for DenyAll {
    fn check(&self, _bearer: Option<&str>) -> Result<(), Denied> {
        Err(Denied)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("--bind must be a loopback address, got {0}")]
    NotLoopback(SocketAddr),
    #[error("cannot bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("--public-url {0:?} is not an absolute http(s) URL")]
    PublicUrl(String),
}

pub struct HttpOptions {
    /// The URL clients reach the server at, with no trailing slash. Used in the
    /// `WWW-Authenticate` challenge and to allow its `Host`.
    pub public_url: String,
}

impl HttpOptions {
    /// `public_url` defaults to `http://<bind>`.
    pub fn new(bind: SocketAddr, public_url: Option<&str>) -> Result<Self, HttpError> {
        let url = match public_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => format!("http://{bind}"),
        };
        public_authority(&url)?;
        Ok(Self { public_url: url })
    }

    pub fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.public_url)
    }
}

fn public_authority(url: &str) -> Result<String, HttpError> {
    let uri: Uri = url
        .parse()
        .map_err(|_| HttpError::PublicUrl(url.to_owned()))?;
    match (uri.scheme_str(), uri.authority()) {
        (Some("http" | "https"), Some(authority)) => Ok(authority.to_string()),
        _ => Err(HttpError::PublicUrl(url.to_owned())),
    }
}

/// Binds `addr`, refusing anything that is not a loopback address.
pub async fn bind_loopback(addr: SocketAddr) -> Result<TcpListener, HttpError> {
    if !addr.ip().is_loopback() {
        return Err(HttpError::NotLoopback(addr));
    }
    TcpListener::bind(addr)
        .await
        .map_err(|source| HttpError::Bind { addr, source })
}

#[derive(Clone)]
struct AuthState {
    check: Arc<dyn TokenCheck>,
    challenge: HeaderValue,
}

/// The `/mcp` route behind the auth layer. Every MCP session gets a clone of `handler`, so all
/// of them share one `Inner`: the active session, audit log, expiry and tunnel config.
/// Cancelling `shutdown` ends the open MCP sessions. Other routes (U2's OAuth endpoints) are
/// merged next to this one, outside the auth layer.
pub fn mcp_router(
    handler: HerdrMcp,
    options: &HttpOptions,
    check: Arc<dyn TokenCheck>,
    shutdown: CancellationToken,
) -> Result<Router, HttpError> {
    let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].map(str::to_owned).into();
    let authority = public_authority(&options.public_url)?;
    hosts.push(authority.clone());
    if let Some((host, _port)) = authority.rsplit_once(':') {
        hosts.push(host.to_owned());
    }
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        .with_cancellation_token(shutdown);
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let challenge = HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{}\"",
        options.resource_metadata_url()
    ))
    .map_err(|_| HttpError::PublicUrl(options.public_url.clone()))?;
    let state = AuthState { check, challenge };
    Ok(Router::new()
        .route_service(MCP_PATH, service)
        .layer(middleware::from_fn_with_state(state, require_token)))
}

async fn require_token(State(state): State<AuthState>, request: Request, next: Next) -> Response {
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        })
        .filter(|token| !token.is_empty());
    match state.check.check(bearer) {
        Ok(()) => next.run(request).await,
        Err(Denied) => (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, state.challenge)],
        )
            .into_response(),
    }
}

/// Serves `router` on `listener` until `shutdown` is cancelled.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
}

/// Cancels `token` on SIGTERM or SIGINT.
pub async fn cancel_on_signal(token: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    token.cancel();
}
