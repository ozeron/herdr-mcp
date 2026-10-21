//! OAuth resource server: verifies a hosted provider's JWT access tokens.
//!
//! The provider (the issuer) authenticates the owner and signs the token. [`JwtCheck`] accepts a
//! token only when its signature matches the issuer's JWKS, `iss`, `aud`, `exp` and `nbf` hold,
//! and it names an owner. It never logs tokens or claims; a denial logs the reason only.

use std::collections::HashMap;
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
/// Cached keys outlive a failing issuer this long after they were loaded.
const JWKS_STALE_CAP: Duration = Duration::from_secs(60 * 60);
const REFETCH_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds discovery plus JWKS, so the cache lock is not held for several HTTP timeouts.
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);
const DENIAL_WINDOW: Duration = Duration::from_secs(60);
const DISCOVERY_PATHS: [&str; 2] = [
    "/.well-known/openid-configuration",
    "/.well-known/oauth-authorization-server",
];
const LEEWAY_SECS: u64 = 60;
/// The prefix of every denial line on stderr; `tunnel ngrok status` reads it back.
pub const DENIED_PREFIX: &str = "auth denied:";
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
    ttl: Duration,
    stale_cap: Duration,
    client: reqwest::Client,
    cache: Mutex<Cache>,
    denials: std::sync::Mutex<DenialLog>,
}

#[derive(Deserialize)]
struct Claims {
    sub: Option<String>,
    email: Option<String>,
    email_verified: Option<bool>,
}

/// Decides which denial lines reach stderr: one per reason per window, with a count of the rest.
#[derive(Default)]
pub struct DenialLog {
    seen: HashMap<&'static str, (Instant, u64)>,
}

impl DenialLog {
    pub fn line(&mut self, reason: &'static str, now: Instant) -> Option<String> {
        let suppressed = match self.seen.get_mut(reason) {
            Some((printed, count)) if now.duration_since(*printed) < DENIAL_WINDOW => {
                *count += 1;
                return None;
            }
            Some((printed, count)) => {
                *printed = now;
                std::mem::take(count)
            }
            None => {
                self.seen.insert(reason, (now, 0));
                0
            }
        };
        Some(if suppressed == 0 {
            format!("herdr-mcp: {DENIED_PREFIX} {reason}")
        } else {
            format!("herdr-mcp: {DENIED_PREFIX} {reason} ({suppressed} suppressed)")
        })
    }
}

impl JwtCheck {
    #[doc(hidden)]
    pub fn with_cache_timing(mut self, ttl: Duration, stale_cap: Duration) -> Self {
        self.ttl = ttl;
        self.stale_cap = stale_cap;
        self
    }

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
        if !allow_http {
            validate_https("--oauth-issuer", issuer)?;
        }
        let client = http_client(HTTP_TIMEOUT).map_err(OAuthError::Client)?;
        Ok(Self {
            issuer: issuer.to_owned(),
            audience: audience.to_owned(),
            owners: owners
                .into_iter()
                .filter(|o| !o.trim().is_empty())
                .collect(),
            allow_http,
            ttl: JWKS_TTL,
            stale_cap: JWKS_STALE_CAP,
            client,
            cache: Mutex::new(Cache::default()),
            denials: std::sync::Mutex::new(DenialLog::default()),
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
        let age = cache.loaded_at.map(|at| at.elapsed());
        let fresh = age.is_some_and(|age| age < self.ttl);
        let usable = age.is_some_and(|age| age < self.stale_cap);
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
        if throttled {
            return match (fresh, usable) {
                (true, _) => Err("unknown_kid"),
                (false, true) => find(&cache).ok_or("unknown_kid"),
                (false, false) => Err("jwks_unavailable"),
            };
        }
        if fresh {
            cache.last_miss = Some(Instant::now());
        }
        let fetched = tokio::time::timeout(FETCH_TIMEOUT, self.fetch_jwks())
            .await
            .unwrap_or(Err("jwks_unavailable"));
        match fetched {
            Ok(set) => {
                cache.keys = Some(set);
                cache.loaded_at = Some(Instant::now());
            }
            Err(reason) => {
                cache.last_miss = Some(Instant::now());
                if !usable {
                    return Err(reason);
                }
            }
        }
        find(&cache).ok_or("unknown_kid")
    }

    async fn fetch_jwks(&self) -> Result<JwkSet, &'static str> {
        let (doc, _) = fetch_discovery(&self.client, &self.issuer)
            .await
            .map_err(|_| "jwks_unavailable")?;
        if doc.get("issuer").and_then(Value::as_str) != Some(self.issuer.as_str()) {
            return Err("jwks_unavailable");
        }
        let uri = doc
            .get("jwks_uri")
            .and_then(Value::as_str)
            .ok_or("jwks_unavailable")?;
        if !self.allow_http && !uri.starts_with("https://") {
            return Err("jwks_unavailable");
        }
        let response = self
            .client
            .get(uri)
            .send()
            .await
            .map_err(|_| "jwks_unavailable")?;
        if !response.status().is_success() {
            return Err("jwks_unavailable");
        }
        response.json().await.map_err(|_| "jwks_unavailable")
    }
}

/// An `https://` URL with a host and no whitespace, trimmed.
pub fn validate_https(what: &'static str, raw: &str) -> Result<String, OAuthError> {
    let url = raw.trim();
    let host = url.strip_prefix("https://").unwrap_or_default();
    if host.is_empty() || url.contains(char::is_whitespace) || host.starts_with('/') {
        return Err(OAuthError::NotHttps {
            what,
            url: raw.to_owned(),
        });
    }
    Ok(url.to_owned())
}

/// A client that follows no redirects and gives up after `timeout`.
pub fn http_client(timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// The issuer's discovery document and the URL it came from: OpenID configuration first, then
/// the OAuth authorization-server metadata. The document's own `issuer` is not checked here.
pub async fn fetch_discovery(
    client: &reqwest::Client,
    issuer: &str,
) -> Result<(Value, String), String> {
    let base = issuer.trim_end_matches('/');
    let mut last = String::new();
    for path in DISCOVERY_PATHS {
        let url = format!("{base}{path}");
        let attempt = async {
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|err| format!("{url}: {err}"))?;
            if !response.status().is_success() {
                return Err(format!("{url} returned {}", response.status()));
            }
            response
                .json::<Value>()
                .await
                .map_err(|err| format!("{url}: {err}"))
        };
        match attempt.await {
            Ok(body) if body.is_object() => return Ok((body, url)),
            Ok(_) => last = format!("{url}: not a JSON object"),
            Err(why) => last = why,
        }
    }
    Err(last)
}

impl TokenCheck for JwtCheck {
    fn check<'a>(&'a self, bearer: Option<&'a str>) -> CheckFuture<'a> {
        Box::pin(async move {
            let Some(token) = bearer else {
                return Err(Denied);
            };
            self.verify(token).await.map_err(|reason| {
                let line = self
                    .denials
                    .lock()
                    .ok()
                    .and_then(|mut log| log.line(reason, Instant::now()));
                if let Some(line) = line {
                    eprintln!("{line}");
                }
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
