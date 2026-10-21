//! OAuth resource server: verifies a hosted provider's JWT access tokens.
//!
//! The provider (the issuer) authenticates the owner and signs the token. [`JwtCheck`] accepts a
//! token only when its signature matches the issuer's JWKS, `iss`, `aud`, `exp` and `nbf` hold,
//! and it names an owner. It never logs tokens or claims; a denial logs the reason only.

use std::time::{Duration, Instant};

use axum::Router;
use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, errors::ErrorKind};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::http::{CheckFuture, Denied, TokenCheck};

const JWKS_TTL: Duration = Duration::from_secs(10 * 60);
const REFETCH_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const LEEWAY_SECS: u64 = 60;
const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// RS256/384/512, ES256/384 and EdDSA. Everything else, including `none` and HS*, is refused.
const ALGORITHMS: [Algorithm; 6] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("--oauth-issuer needs at least one --owner")]
    NoOwner,
    #[error("{what} {url:?} must be an https URL")]
    NotHttps { what: &'static str, url: String },
    #[error("cannot build the HTTP client: {0}")]
    Client(reqwest::Error),
}

#[derive(Default)]
struct Cache {
    keys: Option<JwkSet>,
    loaded_at: Option<Instant>,
    /// Last refetch triggered by an unknown `kid`, or a failed load.
    last_miss: Option<Instant>,
}

pub struct JwtCheck {
    issuer: String,
    audience: String,
    owners: Vec<String>,
    allow_http: bool,
    client: reqwest::Client,
    cache: Mutex<Cache>,
}

#[derive(Deserialize)]
struct Claims {
    sub: Option<String>,
    email: Option<String>,
    email_verified: Option<bool>,
}

impl JwtCheck {
    /// Fails closed: no owner, or an issuer that is not https, is an error.
    pub fn new(issuer: &str, audience: &str, owners: Vec<String>) -> Result<Self, OAuthError> {
        Self::build(issuer, audience, owners, false)
    }

    /// Like [`JwtCheck::new`] but lets the issuer and JWKS be plain http. Tests only; the CLI
    /// cannot reach it.
    pub fn new_insecure_for_tests(
        issuer: &str,
        audience: &str,
        owners: Vec<String>,
    ) -> Result<Self, OAuthError> {
        Self::build(issuer, audience, owners, true)
    }

    fn build(
        issuer: &str,
        audience: &str,
        owners: Vec<String>,
        allow_http: bool,
    ) -> Result<Self, OAuthError> {
        if owners.iter().all(|owner| owner.trim().is_empty()) {
            return Err(OAuthError::NoOwner);
        }
        if !allow_http && !issuer.starts_with("https://") {
            return Err(OAuthError::NotHttps {
                what: "--oauth-issuer",
                url: issuer.to_owned(),
            });
        }
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(OAuthError::Client)?;
        Ok(Self {
            issuer: issuer.to_owned(),
            audience: audience.to_owned(),
            owners: owners
                .into_iter()
                .filter(|o| !o.trim().is_empty())
                .collect(),
            allow_http,
            client,
            cache: Mutex::new(Cache::default()),
        })
    }

    async fn verify(&self, token: &str) -> Result<(), &'static str> {
        let header = decode_header(token).map_err(|_| "invalid_token")?;
        if !ALGORITHMS.contains(&header.alg) {
            return Err("invalid_algorithm");
        }
        let kid = header.kid.ok_or("missing_kid")?;
        let jwk = self.key_for(&kid).await?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| "invalid_key")?;

        let mut validation = Validation::new(header.alg);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let data = decode::<Claims>(token, &key, &validation).map_err(|err| match err.kind() {
            ErrorKind::InvalidIssuer => "invalid_issuer",
            ErrorKind::InvalidAudience => "invalid_audience",
            ErrorKind::ExpiredSignature => "expired",
            ErrorKind::ImmatureSignature => "not_yet_valid",
            ErrorKind::InvalidSignature => "bad_signature",
            _ => "invalid_token",
        })?;
        self.is_owner(&data.claims).then_some(()).ok_or("not_owner")
    }

    fn is_owner(&self, claims: &Claims) -> bool {
        self.owners.iter().any(|owner| {
            claims.sub.as_deref() == Some(owner)
                || (owner.contains('@')
                    && claims.email_verified == Some(true)
                    && claims
                        .email
                        .as_deref()
                        .is_some_and(|email| email.eq_ignore_ascii_case(owner)))
        })
    }

    async fn key_for(&self, kid: &str) -> Result<jsonwebtoken::jwk::Jwk, &'static str> {
        let mut cache = self.cache.lock().await;
        let fresh = cache.loaded_at.is_some_and(|at| at.elapsed() < JWKS_TTL);
        let find = |cache: &Cache| {
            cache
                .keys
                .as_ref()
                .and_then(|set| {
                    set.keys
                        .iter()
                        .find(|k| k.common.key_id.as_deref() == Some(kid))
                })
                .cloned()
        };
        if fresh && let Some(jwk) = find(&cache) {
            return Ok(jwk);
        }
        let throttled = cache
            .last_miss
            .is_some_and(|at| at.elapsed() < REFETCH_INTERVAL);
        if throttled && (fresh || cache.keys.is_none()) {
            return Err(if fresh {
                "unknown_kid"
            } else {
                "jwks_unavailable"
            });
        }
        if fresh {
            cache.last_miss = Some(Instant::now());
        }
        match self.fetch_jwks().await {
            Ok(set) => {
                cache.keys = Some(set);
                cache.loaded_at = Some(Instant::now());
            }
            Err(reason) => {
                cache.last_miss = Some(Instant::now());
                cache.keys = None;
                cache.loaded_at = None;
                return Err(reason);
            }
        }
        find(&cache).ok_or("unknown_kid")
    }

    async fn fetch_jwks(&self) -> Result<JwkSet, &'static str> {
        let base = self.issuer.trim_end_matches('/');
        let mut uri = None;
        for name in ["openid-configuration", "oauth-authorization-server"] {
            if let Some(found) = self.discover(&format!("{base}/.well-known/{name}")).await {
                uri = Some(found);
                break;
            }
        }
        let uri = uri.ok_or("jwks_unavailable")?;
        if !self.allow_http && !uri.starts_with("https://") {
            return Err("jwks_unavailable");
        }
        let response = self
            .client
            .get(&uri)
            .send()
            .await
            .map_err(|_| "jwks_unavailable")?;
        if !response.status().is_success() {
            return Err("jwks_unavailable");
        }
        response.json().await.map_err(|_| "jwks_unavailable")
    }

    /// The `jwks_uri` of the discovery document at `url`, if it is for this issuer.
    async fn discover(&self, url: &str) -> Option<String> {
        let response = self.client.get(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let doc: Value = response.json().await.ok()?;
        if doc.get("issuer").and_then(Value::as_str) != Some(self.issuer.as_str()) {
            return None;
        }
        doc.get("jwks_uri")?.as_str().map(str::to_owned)
    }
}

impl TokenCheck for JwtCheck {
    fn check<'a>(&'a self, bearer: Option<&'a str>) -> CheckFuture<'a> {
        Box::pin(async move {
            let Some(token) = bearer else {
                return Err(Denied);
            };
            self.verify(token).await.map_err(|reason| {
                eprintln!("herdr-mcp: auth denied: {reason}");
                Denied
            })
        })
    }
}

/// `/.well-known/oauth-protected-resource` (also under `/mcp`), served without auth.
pub fn metadata_router(resource: &str, issuer: &str) -> Router {
    let body = json!({
        "resource": resource,
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
    })
    .to_string();
    let handler = move || {
        let body = body.clone();
        async move { ([(CONTENT_TYPE, "application/json")], body) }
    };
    Router::new()
        .route(METADATA_PATH, get(handler.clone()))
        .route(&format!("{METADATA_PATH}/mcp"), get(handler))
}
