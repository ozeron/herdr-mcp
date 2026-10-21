#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fake_herdr::FakeHerdr;
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::herdr::SessionEnv;
use herdr_mcp::http::{
    self, CheckFuture, Denied, DenyAll, HttpError, HttpOptions, MCP_PATH, TokenCheck, bind_loopback,
};
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use herdr_mcp::tunnel::{Stopper, TunnelConfig};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-token";

/// Accepts one fixed bearer token.
struct FixedToken;

impl TokenCheck for FixedToken {
    fn check<'a>(&'a self, bearer: Option<&'a str>) -> CheckFuture<'a> {
        let result = if bearer == Some(TOKEN) {
            Ok(())
        } else {
            Err(Denied)
        };
        Box::pin(async move { result })
    }
}

struct NoStop;

impl Stopper for NoStop {
    fn stop(&self) {}
}

struct Rig {
    addr: SocketAddr,
    default: FakeHerdr,
    alpha: FakeHerdr,
    xdg: TempDir,
    state: TempDir,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    _dir: TempDir,
}

impl Rig {
    fn url(&self) -> String {
        format!("http://{}{MCP_PATH}", self.addr)
    }

    fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.path().join("herdr-mcp/tunnel-audit.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn session_dir(xdg: &Path, name: &str) -> PathBuf {
    let dir = xdg.join("herdr/sessions").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script_list(fake: &FakeHerdr, workspace: &str) {
    let tab = format!("{workspace}:t1");
    let pane = format!("{workspace}:p1");
    let snapshot = json!({
        "version": "0.9.3", "protocol": 22, "layouts": [],
        "focused_workspace_id": workspace, "focused_tab_id": tab, "focused_pane_id": pane,
        "workspaces": [harness::workspace(workspace, 1, &tab)],
        "tabs": [harness::tab(&tab, workspace, 1)],
        "panes": [harness::pane(&pane, &tab, workspace, None)],
        "agents": [],
    });
    fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": snapshot}),
    );
    fake.reply("agent.list", json!({"type": "agent_list", "agents": []}));
}

async fn rig(check: Arc<dyn TokenCheck>, tunnel: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let xdg = tempfile::Builder::new().prefix("x").tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg.path().join("herdr")).unwrap();
    let default = FakeHerdr::start_at(xdg.path().join("herdr/herdr.sock")).await;
    let alpha = FakeHerdr::start_at(session_dir(xdg.path(), "alpha").join("herdr.sock")).await;
    // The home session resolves the caller first, which reads a second snapshot.
    script_list(&default, "w1");
    script_list(&default, "w1");
    script_list(&alpha, "w9");
    let config = BridgeConfig {
        socket_path: default.path().to_path_buf(),
        profile: Profile::new(None, None),
        mode: ToolMode::Read,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv::default(),
        logger: Logger::to_dir(dir.path().join("logs")).unwrap(),
        install_env: None,
    };
    let mut handler = HerdrMcp::new(config)
        .with_session_env(SessionEnv {
            xdg_config_home: Some(xdg.path().to_path_buf()),
            ..SessionEnv::default()
        })
        .with_mailbox_state_dir(dir.path().join("mailbox"));
    if tunnel {
        handler = handler.with_tunnel(TunnelConfig {
            state_home: state.path().to_path_buf(),
            expires_at: None,
            stopper: Arc::new(NoStop),
            start_command: "herdr-mcp tunnel start",
        });
    }
    let listener = bind_loopback("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let options = HttpOptions::new(addr, None).unwrap();
    let shutdown = CancellationToken::new();
    let router = http::mcp_router(handler, &options, check, shutdown.clone()).unwrap();
    let server = tokio::spawn(http::serve(listener, router, shutdown.clone()));
    Rig {
        addr,
        default,
        alpha,
        xdg,
        state,
        shutdown,
        server,
        _dir: dir,
    }
}

/// One MCP client session over HTTP.
struct Mcp {
    http: reqwest::Client,
    url: String,
    session: Option<String>,
    next_id: u64,
}

impl Mcp {
    async fn connect(r: &Rig) -> Self {
        let mut mcp = Self {
            http: reqwest::Client::new(),
            url: r.url(),
            session: None,
            next_id: 1,
        };
        let init = mcp
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "http-test", "version": "1"},
                }),
            )
            .await;
        assert!(init["result"]["serverInfo"].is_object(), "{init}");
        assert!(mcp.session.is_some(), "initialize returns a session id");
        let status = mcp
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await
            .status();
        assert_eq!(status, 202);
        mcp
    }

    async fn post(&self, body: Value) -> reqwest::Response {
        let mut req = self
            .http
            .post(&self.url)
            .bearer_auth(TOKEN)
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(session) = &self.session {
            req = req
                .header("mcp-session-id", session)
                .header("mcp-protocol-version", "2025-06-18");
        }
        req.send().await.unwrap()
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        assert_eq!(response.status(), 200, "{method}");
        if let Some(session) = response.headers().get("mcp-session-id") {
            self.session = Some(session.to_str().unwrap().to_owned());
        }
        let text = response.text().await.unwrap();
        // The reply is a JSON body or an SSE stream whose `data:` lines carry JSON.
        std::iter::once(text.as_str())
            .chain(text.lines().filter_map(|line| line.strip_prefix("data:")))
            .filter_map(|chunk| serde_json::from_str::<Value>(chunk.trim()).ok())
            .find(|value| value["id"] == id)
            .unwrap_or_else(|| panic!("no reply to {method} in {text:?}"))
    }

    async fn tool(&mut self, name: &str, args: Value) -> Value {
        let reply = self
            .rpc("tools/call", json!({"name": name, "arguments": args}))
            .await;
        assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
        reply["result"]["structuredContent"].clone()
    }
}

#[tokio::test]
async fn no_token_gets_401_with_the_resource_metadata_challenge() {
    let r = rig(Arc::new(DenyAll), false).await;
    let client = reqwest::Client::new();
    let challenge = format!(
        "Bearer resource_metadata=\"http://{}/.well-known/oauth-protected-resource\"",
        r.addr
    );

    let bare = client
        .post(r.url())
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 401);
    assert_eq!(bare.headers()["www-authenticate"], challenge.as_str());

    // DenyAll refuses even a bearer token: the CLI has no way to turn auth off.
    let with_token = client
        .post(r.url())
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(with_token.status(), 401);
    assert_eq!(r.default.requests().len(), 0);
}

#[tokio::test]
async fn a_wrong_token_or_scheme_gets_401() {
    let r = rig(Arc::new(FixedToken), false).await;
    let client = reqwest::Client::new();

    let wrong = client
        .post(r.url())
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    let basic = client
        .post(r.url())
        .header("authorization", format!("Basic {TOKEN}"))
        .send()
        .await
        .unwrap();

    assert_eq!(wrong.status(), 401);
    assert_eq!(basic.status(), 401);
    assert!(wrong.headers().contains_key("www-authenticate"));
}

#[tokio::test]
async fn initialize_list_and_call_work_over_http_with_a_token() {
    let r = rig(Arc::new(FixedToken), false).await;
    let mut mcp = Mcp::connect(&r).await;

    let tools = mcp.rpc("tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"herdr_list"), "{names:?}");
    assert!(!names.contains(&"herdr_tunnel_stop"));

    let listed = mcp.tool("herdr_list", json!({})).await;
    assert_eq!(listed["session"], "default");
    assert_eq!(harness::count(&r.default, "session.snapshot"), 2);
}

#[tokio::test]
async fn two_mcp_sessions_share_the_active_session() {
    let r = rig(Arc::new(FixedToken), false).await;
    let mut first = Mcp::connect(&r).await;
    let mut second = Mcp::connect(&r).await;
    assert_ne!(first.session, second.session);

    let switched = first
        .tool("herdr_use_session", json!({"name": "alpha"}))
        .await;
    assert_eq!(switched["active"], "alpha");

    let listed = second.tool("herdr_list", json!({})).await;
    assert_eq!(listed["session"], "alpha");
    assert_eq!(harness::count(&r.alpha, "session.snapshot"), 1);
    assert_eq!(harness::count(&r.default, "session.snapshot"), 0);
    let _ = &r.xdg;
}

#[tokio::test]
async fn tunnel_mode_audits_calls_over_http() {
    let r = rig(Arc::new(FixedToken), true).await;
    let mut mcp = Mcp::connect(&r).await;

    let tools = mcp.rpc("tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"herdr_tunnel_stop"));

    mcp.tool("herdr_list", json!({})).await;

    let lines = r.audit_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["tool"], "herdr_list");
    assert_eq!(lines[0]["outcome"], "ok");
    let session: String = mcp.session.as_deref().unwrap().chars().take(8).collect();
    assert_eq!(lines[0]["mcp_session"], session.as_str());
    assert!(lines[0]["panes"].is_u64(), "{}", lines[0]);
}

#[tokio::test]
async fn shutdown_ends_the_server() {
    let r = rig(Arc::new(FixedToken), false).await;
    let _mcp = Mcp::connect(&r).await;

    r.shutdown.cancel();

    let done = tokio::time::timeout(std::time::Duration::from_secs(5), r.server).await;
    assert!(done.expect("server stops after shutdown").unwrap().is_ok());
}

#[tokio::test]
async fn a_non_loopback_bind_is_refused() {
    for addr in ["0.0.0.0:0", "192.168.1.5:0", "[::]:0"] {
        let err = bind_loopback(addr.parse().unwrap()).await.unwrap_err();
        assert!(matches!(err, HttpError::NotLoopback(_)), "{addr}: {err}");
    }
}

#[test]
fn the_binary_refuses_a_non_loopback_bind() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-mcp"))
        .args(["http-serve", "--bind", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}

#[test]
fn the_cli_has_no_flag_that_turns_auth_off() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-mcp"))
        .args(["http-serve", "--no-auth"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
