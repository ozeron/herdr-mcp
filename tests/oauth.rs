#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fake_herdr::FakeHerdr;
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::http::{self, HttpOptions, MCP_PATH, TokenCheck, bind_loopback};
use herdr_mcp::log::Logger;
use herdr_mcp::oauth::{self, DenialLog, JwtCheck};
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const KEY_A: &str = include_str!("fixtures/oauth-test-a.pem");
const PUB_A: &str = include_str!("fixtures/oauth-test-a.pub.pem");
const JWK_A: &str = include_str!("fixtures/oauth-test-a.jwk.json");
const KEY_B: &str = include_str!("fixtures/oauth-test-b.pem");
const JWK_B: &str = include_str!("fixtures/oauth-test-b.jwk.json");
const AUDIENCE: &str = "https://herdr.example.test/mcp";
const OWNER_SUB: &str = "user_owner";
const OWNER_EMAIL: &str = "owner@example.test";

#[derive(Default)]
struct IssuerState {
    jwks: Value,
    jwks_hits: usize,
    oidc_missing: bool,
    jwks_down: bool,
    hang: bool,
    named_issuer: Option<String>,
}

struct Issuer {
    url: String,
    state: Arc<Mutex<IssuerState>>,
}

impl Issuer {
    fn set_key(&self, jwk: &str, kid: &str) {
        let mut jwk: Value = serde_json::from_str(jwk).unwrap();
        jwk["kid"] = json!(kid);
        jwk["alg"] = json!("RS256");
        jwk["use"] = json!("sig");
        self.state.lock().unwrap().jwks = json!({"keys": [jwk]});
    }

    fn set_jwks_down(&self, down: bool) {
        self.state.lock().unwrap().jwks_down = down;
    }

    fn jwks_hits(&self) -> usize {
        self.state.lock().unwrap().jwks_hits
    }
}

async fn discovery(State(s): State<(String, Arc<Mutex<IssuerState>>)>) -> impl IntoResponse {
    let (hang, named) = {
        let state = s.1.lock().unwrap();
        (state.hang, state.named_issuer.clone())
    };
    if hang {
        tokio::time::sleep(Duration::from_secs(120)).await;
    }
    let body = json!({
        "issuer": named.unwrap_or_else(|| s.0.clone()),
        "jwks_uri": format!("{}/jwks", s.0),
    });
    ([(CONTENT_TYPE, "application/json")], body.to_string())
}

async fn openid(State(s): State<(String, Arc<Mutex<IssuerState>>)>) -> axum::response::Response {
    if s.1.lock().unwrap().oidc_missing {
        return StatusCode::NOT_FOUND.into_response();
    }
    discovery(State(s)).await.into_response()
}

async fn jwks(State(s): State<(String, Arc<Mutex<IssuerState>>)>) -> axum::response::Response {
    let mut state = s.1.lock().unwrap();
    state.jwks_hits += 1;
    if state.jwks_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    ([(CONTENT_TYPE, "application/json")], state.jwks.to_string()).into_response()
}

async fn issuer(oidc_missing: bool) -> Issuer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(IssuerState {
        oidc_missing,
        ..IssuerState::default()
    }));
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(openid))
        .route("/.well-known/oauth-authorization-server", get(discovery))
        .route("/jwks", get(jwks))
        .with_state((url.clone(), state.clone()));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let issuer = Issuer { url, state };
    issuer.set_key(JWK_A, "kid-a");
    issuer
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(iss: &str) -> Value {
    json!({
        "iss": iss, "aud": AUDIENCE, "sub": OWNER_SUB,
        "exp": now() + 600, "nbf": now() - 10,
    })
}

fn sign(alg: Algorithm, kid: &str, key: &EncodingKey, claims: &Value) -> String {
    let mut header = Header::new(alg);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(&header, claims, key).unwrap()
}

fn rs256_without_kid(pem: &str, claims: &Value) -> String {
    jsonwebtoken::encode(
        &Header::new(Algorithm::RS256),
        claims,
        &EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn rs256(kid: &str, pem: &str, claims: &Value) -> String {
    sign(
        Algorithm::RS256,
        kid,
        &EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        claims,
    )
}

fn alg_none(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!(
        "{}.{}.",
        part(&json!({"alg": "none", "typ": "JWT", "kid": "kid-a"})),
        part(claims)
    )
}

struct Rig {
    addr: SocketAddr,
    _dir: TempDir,
    _state: TempDir,
    _fake: FakeHerdr,
}

impl Rig {
    fn url(&self) -> String {
        format!("http://{}{MCP_PATH}", self.addr)
    }

    async fn initialize(&self, bearer: Option<&str>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .post(self.url())
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "oauth-test", "version": "1"},
                },
            }));
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        req.send().await.unwrap()
    }

    async fn assert_ok(&self, token: &str) {
        let response = self.initialize(Some(token)).await;
        assert_eq!(response.status(), 200);
    }

    async fn assert_denied(&self, bearer: Option<&str>, why: &str) {
        let response = self.initialize(bearer).await;
        assert_eq!(response.status(), 401, "{why}");
        let challenge = response.headers()["www-authenticate"].to_str().unwrap();
        assert_eq!(
            challenge,
            format!(
                "Bearer resource_metadata=\"http://{}/.well-known/oauth-protected-resource\"",
                self.addr
            ),
            "{why}"
        );
    }
}

fn check_for(issuer: &Issuer) -> Arc<JwtCheck> {
    Arc::new(
        JwtCheck::new_insecure_for_tests(
            &issuer.url,
            AUDIENCE,
            vec![OWNER_SUB.to_owned(), OWNER_EMAIL.to_owned()],
        )
        .unwrap(),
    )
}

async fn rig(issuer: &Issuer) -> Rig {
    rig_with(issuer, check_for(issuer)).await
}

async fn rig_with(issuer: &Issuer, check: Arc<JwtCheck>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let fake = FakeHerdr::start_at(dir.path().join("herdr.sock")).await;
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, None),
        mode: ToolMode::Read,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv::default(),
        logger: Logger::to_dir(dir.path().join("logs")).unwrap(),
        install_env: None,
    };
    let handler = HerdrMcp::new(config).with_mailbox_state_dir(dir.path().join("mailbox"));
    let listener = bind_loopback("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let options = HttpOptions::new(addr, None).unwrap();
    let shutdown = CancellationToken::new();
    let check: Arc<dyn TokenCheck> = check;
    let router = http::mcp_router(handler, &options, check, shutdown.clone())
        .unwrap()
        .merge(oauth::metadata_router(AUDIENCE, &issuer.url));
    tokio::spawn(http::serve(listener, router, shutdown));
    Rig {
        addr,
        _dir: dir,
        _state: state,
        _fake: fake,
    }
}

#[tokio::test]
async fn an_owner_token_by_sub_or_verified_email_gets_in() {
    let iss = issuer(false).await;
    let r = rig(&iss).await;

    r.assert_ok(&rs256("kid-a", KEY_A, &claims(&iss.url))).await;

    let mut by_email = claims(&iss.url);
    by_email["sub"] = json!("someone_else");
    by_email["email"] = json!(OWNER_EMAIL);
    by_email["email_verified"] = json!(true);
    r.assert_ok(&rs256("kid-a", KEY_A, &by_email)).await;
}

#[tokio::test]
async fn discovery_falls_back_to_the_oauth_authorization_server_document() {
    let iss = issuer(true).await;
    let r = rig(&iss).await;
    r.assert_ok(&rs256("kid-a", KEY_A, &claims(&iss.url))).await;
}

#[tokio::test]
async fn bad_tokens_get_401_with_the_resource_metadata_challenge() {
    let iss = issuer(false).await;
    let r = rig(&iss).await;
    let good = claims(&iss.url);

    r.assert_denied(None, "no token").await;
    r.assert_denied(Some("not-a-jwt"), "garbage").await;

    let mut c = good.clone();
    c["iss"] = json!("https://evil.example.test");
    r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), "wrong iss")
        .await;

    let mut c = good.clone();
    c["aud"] = json!("https://other.example.test/mcp");
    r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), "wrong aud")
        .await;

    let mut c = good.clone();
    c["exp"] = json!(now() - 3600);
    r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), "expired")
        .await;

    let mut c = good.clone();
    c["nbf"] = json!(now() + 3600);
    r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), "nbf in the future")
        .await;

    r.assert_denied(
        Some(&rs256("kid-a", KEY_B, &good)),
        "signed with another key",
    )
    .await;
    r.assert_denied(Some(&alg_none(&good)), "alg none").await;

    let hs = sign(
        Algorithm::HS256,
        "kid-a",
        &EncodingKey::from_secret(PUB_A.as_bytes()),
        &good,
    );
    r.assert_denied(Some(&hs), "HS256 with the public key")
        .await;

    let mut c = good.clone();
    c["sub"] = json!("user_stranger");
    r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), "not an owner")
        .await;

    let mut c = good.clone();
    c["sub"] = json!("user_stranger");
    c["email"] = json!(OWNER_EMAIL);
    c["email_verified"] = json!(false);
    r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), "unverified email")
        .await;

    let mut c = good.clone();
    c["sub"] = json!("user_stranger");
    c["email"] = json!(OWNER_EMAIL);
    r.assert_denied(
        Some(&rs256("kid-a", KEY_A, &c)),
        "email without email_verified",
    )
    .await;

    r.assert_ok(&rs256("kid-a", KEY_A, &good)).await;
}

#[tokio::test]
async fn an_unknown_kid_refetches_the_jwks_once_then_fails() {
    let iss = issuer(false).await;
    let r = rig(&iss).await;
    let good = claims(&iss.url);

    r.assert_ok(&rs256("kid-a", KEY_A, &good)).await;
    assert_eq!(iss.jwks_hits(), 1);

    iss.set_key(JWK_B, "kid-b");
    r.assert_ok(&rs256("kid-b", KEY_B, &good)).await;
    assert_eq!(iss.jwks_hits(), 2);

    r.assert_denied(Some(&rs256("kid-zzz", KEY_B, &good)), "unknown kid")
        .await;
    assert_eq!(
        iss.jwks_hits(),
        2,
        "a second miss within 30 s does not refetch"
    );
}

#[tokio::test]
async fn tokens_missing_exp_aud_or_kid_are_denied() {
    let iss = issuer(false).await;
    let r = rig(&iss).await;
    let good = claims(&iss.url);

    for claim in ["exp", "aud"] {
        let mut c = good.clone();
        c.as_object_mut().unwrap().remove(claim);
        r.assert_denied(Some(&rs256("kid-a", KEY_A, &c)), claim)
            .await;
    }
    r.assert_denied(Some(&rs256_without_kid(KEY_A, &good)), "no kid")
        .await;
}

#[tokio::test]
async fn owners_match_exactly_by_sub_and_case_insensitively_by_verified_email() {
    let iss = issuer(false).await;
    let r = rig(&iss).await;

    let mut stranger = claims(&iss.url);
    stranger["sub"] = json!("user_stranger");
    stranger["email"] = json!(OWNER_SUB);
    stranger["email_verified"] = json!(true);
    r.assert_denied(
        Some(&rs256("kid-a", KEY_A, &stranger)),
        "a verified email equal to a sub-style owner",
    )
    .await;

    let mut upper = claims(&iss.url);
    upper["sub"] = json!("user_stranger");
    upper["email"] = json!("OWNER@Example.Test");
    upper["email_verified"] = json!(true);
    r.assert_ok(&rs256("kid-a", KEY_A, &upper)).await;
}

#[tokio::test]
async fn a_discovery_document_naming_another_issuer_is_not_trusted() {
    let iss = issuer(false).await;
    iss.state.lock().unwrap().named_issuer = Some("https://evil.example.test".to_owned());
    let r = rig(&iss).await;
    r.assert_denied(
        Some(&rs256("kid-a", KEY_A, &claims(&iss.url))),
        "discovery issuer mismatch",
    )
    .await;
    assert_eq!(iss.jwks_hits(), 0);
}

fn timed_check(iss: &Issuer, ttl: Duration, cap: Duration) -> Arc<JwtCheck> {
    Arc::new(
        JwtCheck::new_insecure_for_tests(&iss.url, AUDIENCE, vec![OWNER_SUB.to_owned()])
            .unwrap()
            .with_cache_timing(ttl, cap),
    )
}

#[tokio::test]
async fn cached_keys_keep_serving_while_the_issuer_is_down_until_the_cap() {
    let iss = issuer(false).await;
    let r = rig_with(
        &iss,
        timed_check(
            &iss,
            Duration::from_millis(100),
            Duration::from_millis(1500),
        ),
    )
    .await;
    let good = rs256("kid-a", KEY_A, &claims(&iss.url));
    r.assert_ok(&good).await;

    iss.set_jwks_down(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    r.assert_ok(&good).await;
    r.assert_denied(
        Some(&rs256("kid-zzz", KEY_B, &claims(&iss.url))),
        "unknown kid while the refetch fails",
    )
    .await;
    r.assert_ok(&good).await;

    tokio::time::sleep(Duration::from_millis(1600)).await;
    r.assert_denied(Some(&good), "past the staleness cap").await;
}

#[tokio::test]
async fn a_hanging_issuer_does_not_hold_the_check_for_long() {
    let iss = issuer(false).await;
    iss.state.lock().unwrap().hang = true;
    let r = rig(&iss).await;
    let token = rs256("kid-a", KEY_A, &claims(&iss.url));
    let started = Instant::now();
    r.assert_denied(Some(&token), "issuer hangs").await;
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_burst_of_one_denial_reason_prints_once_per_window() {
    let mut log = DenialLog::default();
    let t = Instant::now();
    let secs = Duration::from_secs;
    let first = log.line("not_owner", t).expect("the first prints");
    assert!(first.ends_with("auth denied: not_owner"), "{first}");
    for i in 1..=5 {
        assert!(log.line("not_owner", t + secs(i)).is_none());
    }
    assert!(log.line("expired", t + secs(6)).is_some());
    let next = log.line("not_owner", t + secs(61)).expect("window over");
    assert!(next.contains("auth denied: not_owner"), "{next}");
    assert!(next.contains("5 suppressed"), "{next}");
    assert!(log.line("not_owner", t + secs(62)).is_none());
}

#[tokio::test]
async fn the_metadata_document_names_the_issuer_and_the_resource() {
    let iss = issuer(false).await;
    let r = rig(&iss).await;
    for suffix in ["", "/mcp"] {
        let url = format!(
            "http://{}/.well-known/oauth-protected-resource{suffix}",
            r.addr
        );
        let body: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
        assert_eq!(
            body,
            json!({
                "resource": AUDIENCE,
                "authorization_servers": [iss.url],
                "bearer_methods_supported": ["header"],
            })
        );
    }
}

#[test]
fn the_check_refuses_no_owner_and_an_http_issuer() {
    let owners = || vec![OWNER_SUB.to_owned()];
    assert!(JwtCheck::new("https://issuer.example.test", AUDIENCE, vec![]).is_err());
    assert!(JwtCheck::new("http://issuer.example.test", AUDIENCE, owners()).is_err());
    assert!(JwtCheck::new("https://issuer.example.test", AUDIENCE, owners()).is_ok());
}

fn serve_args(extra: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_herdr-mcp"))
        .args(["http-serve", "--oauth-audience", AUDIENCE])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn the_binary_refuses_an_issuer_without_an_owner_or_over_http() {
    let out = serve_args(&["--oauth-issuer", "https://issuer.example.test"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--owner"));

    let out = serve_args(&[
        "--oauth-issuer",
        "http://issuer.example.test",
        "--owner",
        "u",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("https"));
}

#[test]
fn the_stdio_command_has_no_oauth_flags() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-mcp"))
        .args(["--oauth-issuer", "https://issuer.example.test"])
        .args(["--oauth-audience", AUDIENCE, "--owner", "u"])
        .env("HERDR_SOCKET_PATH", "/nonexistent/herdr.sock")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unexpected argument"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
