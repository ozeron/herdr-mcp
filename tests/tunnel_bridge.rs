#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fake_herdr::{FakeHerdr, Script};
use harness::{Harness, call, err, err_code, ok};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use herdr_mcp::tunnel::{Stopper, TunnelConfig};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use serde_json::{Value, json};
use tempfile::TempDir;

const SECRET: &str = "PANE-OUTPUT-SECRET-42";

#[derive(Default)]
struct CountingStopper(AtomicUsize);

impl Stopper for CountingStopper {
    fn stop(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Rig {
    h: Harness,
    state: TempDir,
    stopper: Arc<CountingStopper>,
}

impl Rig {
    fn audit_path(&self) -> PathBuf {
        self.state.path().join("herdr-mcp/tunnel-audit.log")
    }

    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.audit_path())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("each audit line is JSON"))
            .collect()
    }

    fn raw(&self) -> String {
        std::fs::read_to_string(self.audit_path()).unwrap_or_default()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn serve(
    mode: ToolMode,
    tunnel: bool,
    expires_at: Option<u64>,
    state_home: Option<PathBuf>,
) -> Rig {
    serve_as(
        mode,
        tunnel,
        expires_at,
        state_home,
        "herdr-mcp tunnel start",
    )
    .await
}

async fn serve_as(
    mode: ToolMode,
    tunnel: bool,
    expires_at: Option<u64>,
    state_home: Option<PathBuf>,
    start_command: &'static str,
) -> Rig {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let stopper = Arc::new(CountingStopper::default());
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, None),
        mode,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv::default(),
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
        install_env: None,
    };
    let mailbox = log_dir.path().join("mailbox");
    let tunnel_config = TunnelConfig {
        state_home: state_home.unwrap_or_else(|| state.path().to_path_buf()),
        expires_at,
        stopper: stopper.clone(),
        start_command,
    };
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let mut server = HerdrMcp::new(config).with_mailbox_state_dir(mailbox);
        if tunnel {
            server = server.with_tunnel(tunnel_config);
        }
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("test-client", "1.0.0"),
    )
    .serve(client_io)
    .await
    .unwrap();
    Rig {
        h: Harness {
            fake,
            client,
            _log_dir: log_dir,
        },
        state,
        stopper,
    }
}

fn read_result(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": "w1:p4", "workspace_id": "w1", "tab_id": "w1:t1", "source": "recent",
        "format": "text", "text": text, "revision": 7, "truncated": false}})
}

async fn tool_names(r: &Rig) -> Vec<String> {
    r.h.client
        .list_all_tools()
        .await
        .unwrap()
        .iter()
        .map(|t| t.name.to_string())
        .collect()
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn each_call_appends_one_json_line_with_tool_session_target_outcome_and_ms() {
    let r = serve(ToolMode::Read, true, None, None).await;
    r.h.fake.reply("agent.read", read_result(SECRET));
    ok(
        &r.h,
        "herdr_read",
        json!({"target": "w1:p4", "lines": 20, "max_chars": 50}),
    )
    .await;
    ok(&r.h, "herdr_sessions", json!({})).await;

    let lines = r.lines();
    assert_eq!(lines.len(), 2, "{}", r.raw());
    let read = &lines[0];
    assert_eq!(read["tool"], "herdr_read");
    assert_eq!(read["target"], "w1:p4");
    assert_eq!(read["lines"], 20);
    assert_eq!(read["outcome"], "ok");
    assert_eq!(read["session"], "custom");
    assert!(read["ms"].is_u64(), "{read}");
    assert!(read["ts"].as_str().unwrap().ends_with('Z'), "{read}");
    assert_eq!(lines[1]["tool"], "herdr_sessions");
    assert!(lines[1].get("target").is_none());
}

#[tokio::test]
async fn each_line_carries_the_client_a_result_summary_and_the_error() {
    let r = serve(ToolMode::Read, true, None, None).await;
    r.h.fake.reply("agent.read", read_result(SECRET));
    ok(&r.h, "herdr_read", json!({"target": "w1:p4"})).await;
    r.h.fake
        .on("agent.read", Script::error("agent_not_found", "no agent"));
    r.h.fake
        .on("pane.read", Script::error("pane_not_found", "no pane"));
    err_code(&r.h, "herdr_read", json!({"target": "ghost"})).await;

    let lines = r.lines();
    let read = &lines[0];
    assert!(read["client"].is_string(), "{read}");
    assert_eq!(read["source"], "recent");
    assert_eq!(read["chars"], SECRET.chars().count());
    assert!(read.get("error").is_none());
    let failed = &lines[1];
    assert_eq!(failed["outcome"], "target_not_found");
    assert!(
        failed["error"].as_str().unwrap().contains("ghost"),
        "{failed}"
    );
    assert!(!r.raw().contains(SECRET), "{}", r.raw());
}

#[tokio::test]
async fn read_tools_never_log_pane_output_or_results() {
    let r = serve(ToolMode::Read, true, None, None).await;
    r.h.fake.reply("agent.read", read_result(SECRET));
    let read = ok(&r.h, "herdr_read", json!({"target": "w1:p4"})).await;
    assert!(read.to_string().contains(SECRET), "the tool does return it");
    ok(&r.h, "herdr_sessions", json!({})).await;
    err_code(
        &r.h,
        "herdr_wait",
        json!({"target": "w1:p4", "output": {"match": SECRET}}),
    )
    .await;
    assert!(!r.raw().contains(SECRET), "{}", r.raw());
    assert!(r.raw().contains("herdr_wait"));
}

#[tokio::test]
async fn write_tools_log_their_payload() {
    let r = serve(ToolMode::Control, true, None, None).await;
    err_code(&r.h, "herdr_run", json!({"command": "echo run-payload"})).await;
    err_code(
        &r.h,
        "herdr_send",
        json!({"target": "w1:p4", "text": "send-text", "keys": ["Enter"]}),
    )
    .await;
    err_code(
        &r.h,
        "herdr_send",
        json!({"target": "w1:p4", "prompt": "send-prompt"}),
    )
    .await;
    err_code(
        &r.h,
        "herdr_launch",
        json!({"kind": "codex", "name": "worker", "instructions": "do-the-thing",
               "runtime_mode": "full-access"}),
    )
    .await;
    err_code(
        &r.h,
        "herdr_interrupt",
        json!({"target": "w1:p4", "method": "keys"}),
    )
    .await;
    err_code(
        &r.h,
        "herdr_close",
        json!({"target": "w1:p4", "force": true}),
    )
    .await;
    err_code(
        &r.h,
        "herdr_mail_send",
        json!({"to": "w1:p4", "body": "MAIL-BODY-SECRET"}),
    )
    .await;
    ok(&r.h, "herdr_use_session", json!({"name": "custom"})).await;

    let lines = r.lines();
    let by = |tool: &str| -> Vec<&Value> { lines.iter().filter(|l| l["tool"] == tool).collect() };
    assert_eq!(by("herdr_run")[0]["command"], "echo run-payload");
    let sends = by("herdr_send");
    assert_eq!(sends[0]["target"], "w1:p4");
    assert_eq!(sends[0]["text"], "send-text");
    assert_eq!(sends[0]["keys"], json!(["Enter"]));
    assert_eq!(sends[1]["prompt"], "send-prompt");
    let launch = by("herdr_launch")[0];
    assert_eq!(launch["kind"], "codex");
    assert_eq!(launch["name"], "worker");
    assert_eq!(launch["instructions"], "do-the-thing");
    assert_eq!(launch["runtime_mode"], "full-access");
    assert_eq!(by("herdr_interrupt")[0]["method"], "keys");
    let close = by("herdr_close")[0];
    assert_eq!(close["target"], "w1:p4");
    assert_eq!(close["force"], true);
    let mail = by("herdr_mail_send")[0];
    assert_eq!(mail["to"], "w1:p4");
    assert_eq!(mail["body_len"], 16);
    assert!(!r.raw().contains("MAIL-BODY-SECRET"), "{}", r.raw());
    assert_eq!(by("herdr_use_session")[0]["name"], "custom");
    assert_eq!(by("herdr_use_session")[0]["outcome"], "ok");
    assert_ne!(by("herdr_run")[0]["outcome"], "ok", "errors log their code");
}

#[tokio::test]
async fn the_audit_file_is_0600_in_a_0700_dir() {
    let r = serve(ToolMode::Read, true, None, None).await;
    ok(&r.h, "herdr_sessions", json!({})).await;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&r.audit_path()), 0o600);
    assert_eq!(mode(r.audit_path().parent().unwrap()), 0o700);
}

#[tokio::test]
async fn without_tunnel_there_is_no_audit_file_and_no_stop_tool() {
    let r = serve(ToolMode::Control, false, None, None).await;
    ok(&r.h, "herdr_sessions", json!({})).await;
    assert!(!r.audit_path().exists());
    assert!(
        !tool_names(&r)
            .await
            .contains(&"herdr_tunnel_stop".to_string())
    );
    let result =
        r.h.client
            .call_tool(CallToolRequestParams::new("herdr_tunnel_stop"))
            .await;
    assert!(result.is_err(), "{result:?}");
}

#[tokio::test]
async fn tunnel_mode_registers_the_stop_tool_in_both_modes() {
    for (mode, expected) in [(ToolMode::Read, 10), (ToolMode::Control, 16)] {
        let r = serve(mode, true, None, None).await;
        let names = tool_names(&r).await;
        assert!(
            names.contains(&"herdr_tunnel_stop".to_string()),
            "{names:?}"
        );
        assert_eq!(names.len(), expected);
        let tools = r.h.client.list_all_tools().await.unwrap();
        let stop = tools
            .iter()
            .find(|t| t.name == "herdr_tunnel_stop")
            .unwrap();
        let annotations = stop.annotations.as_ref().unwrap();
        assert_eq!(annotations.destructive_hint, Some(true));
    }
}

#[tokio::test]
async fn tunnel_stop_replies_first_then_calls_the_stop_helper_and_is_audited() {
    let r = serve(ToolMode::Read, true, None, None).await;
    let reply = ok(&r.h, "herdr_tunnel_stop", json!({})).await;
    assert_eq!(reply, json!({"stopping": true}));
    eventually("the stop helper", || {
        r.stopper.0.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(r.lines()[0]["tool"], "herdr_tunnel_stop");
}

#[tokio::test]
async fn after_expiry_calls_return_tunnel_expired_and_the_stop_helper_runs() {
    let r = serve(ToolMode::Read, true, Some(now() - 1), None).await;
    assert_eq!(
        err_code(&r.h, "herdr_sessions", json!({})).await,
        "tunnel_expired"
    );
    eventually("the stop helper", || {
        r.stopper.0.load(Ordering::SeqCst) >= 1
    })
    .await;
    let lines = r.lines();
    assert!(
        lines
            .iter()
            .any(|l| l["tool"] == "herdr_sessions" && l["outcome"] == "tunnel_expired"),
        "{}",
        r.raw()
    );
}

#[tokio::test]
async fn the_expiry_message_names_the_command_that_starts_this_service() {
    for command in ["herdr-mcp tunnel start", "herdr-mcp tunnel ngrok start"] {
        let r = serve_as(ToolMode::Read, true, Some(now() - 1), None, command).await;
        let failure = err(&r.h, "herdr_sessions", json!({})).await;
        let message = failure["message"].as_str().unwrap();
        assert!(message.contains(&format!("`{command}`")), "{message}");
    }
}

#[tokio::test]
async fn before_expiry_calls_work_and_expiry_fires_on_time() {
    let r = serve(ToolMode::Read, true, Some(now() + 2), None).await;
    ok(&r.h, "herdr_sessions", json!({})).await;
    assert_eq!(r.stopper.0.load(Ordering::SeqCst), 0);
    eventually("the expiry stop", || {
        r.stopper.0.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        err_code(&r.h, "herdr_sessions", json!({})).await,
        "tunnel_expired"
    );
}

#[tokio::test]
async fn an_unwritable_audit_log_does_not_fail_the_call() {
    let blocker = tempfile::NamedTempFile::new().unwrap();
    let r = serve(
        ToolMode::Read,
        true,
        None,
        Some(blocker.path().join("nested")),
    )
    .await;
    let result = call(&r.h, "herdr_sessions", json!({})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
}

/// Starts the real binary over stdio with `env`, sends initialize and returns its reply.
fn initialize_binary(args: &[&str], env: &[(&str, &Path)]) -> Value {
    use std::io::{BufRead, BufReader, Write};
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-mcp"));
    cmd.args(args)
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_TAB_ID")
        .env_remove("HERDR_WORKSPACE_ID")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"t","version":"0"}}}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn a_tunnel_bridge_ignores_an_inherited_caller_pane() {
    let dir = TempDir::new().unwrap();
    let pane = Path::new("w9:p9");
    let state = dir.path().join("state");
    let socket = dir.path().join("herdr.sock");
    let env = [
        ("HERDR_PANE_ID", pane),
        ("XDG_STATE_HOME", state.as_path()),
        ("HERDR_SOCKET_PATH", socket.as_path()),
    ];

    let tunnel = initialize_binary(&["--tools", "read", "--tunnel"], &env);
    let local = initialize_binary(&["--tools", "read"], &env);

    let text = |reply: &Value| reply["result"]["instructions"].as_str().unwrap().to_owned();
    assert!(
        text(&tunnel).contains("You have no caller pane"),
        "{tunnel}"
    );
    assert!(!text(&local).contains("You have no caller pane"), "{local}");
}

fn script_no_agents(r: &Rig) {
    r.h.fake.on_params(
        "session.snapshot",
        json!({}),
        Script::Reply(harness::snapshot(None)),
    );
    r.h.fake.on_params(
        "agent.get",
        json!({}),
        Script::error("agent_not_found", "no such agent"),
    );
}

#[tokio::test]
async fn each_remote_inbox_is_separate_and_persists_by_slug() {
    let r = serve(ToolMode::Read, true, None, None).await;
    script_no_agents(&r);

    for (to, body) in [("chatgpt", "for chatgpt"), ("claude", "for claude")] {
        let sent = ok(
            &r.h,
            "herdr_mail_send",
            json!({"to": to, "body": body, "inbox": "worker-sim"}),
        )
        .await;
        assert_eq!(sent["to"]["name"], to, "{sent}");
    }

    let chatgpt = ok(&r.h, "herdr_mail_read", json!({"inbox": "chatgpt"})).await;
    let claude = ok(&r.h, "herdr_mail_read", json!({"inbox": "claude"})).await;
    let bodies = |read: &Value| -> Vec<String> {
        read["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(bodies(&chatgpt), ["for chatgpt"]);
    assert_eq!(bodies(&claude), ["for claude"]);
    assert_eq!(chatgpt["messages"][0]["from"]["name"], "worker-sim");
}

#[tokio::test]
async fn the_default_inbox_comes_from_the_client() {
    let r = serve(ToolMode::Read, true, None, None).await;
    script_no_agents(&r);

    ok(
        &r.h,
        "herdr_mail_send",
        json!({"to": "test-client", "body": "hi"}),
    )
    .await;
    let read = ok(&r.h, "herdr_mail_read", json!({})).await;

    assert_eq!(read["messages"][0]["body"], "hi", "{read}");
    assert_eq!(read["messages"][0]["from"]["name"], "test-client");
}

#[tokio::test]
async fn a_remote_inbox_named_like_a_live_agent_is_refused() {
    let r = serve(ToolMode::Read, true, None, None).await;
    r.h.fake.on_params(
        "agent.get",
        json!({}),
        Script::Reply(
            json!({"type": "agent_info", "agent": harness::agent("w1:p4", "w1:t2", "w1", "rev")}),
        ),
    );

    let out = err(&r.h, "herdr_mail_read", json!({"inbox": "rev"})).await;

    assert_eq!(out["code"], "inbox_taken");
    assert_eq!(out["pane"], "w1:p4");
}

#[tokio::test]
async fn a_bad_inbox_slug_is_invalid() {
    let r = serve(ToolMode::Read, true, None, None).await;
    for slug in ["", "Has Caps", "-lead", "a/b"] {
        let out = err(&r.h, "herdr_mail_read", json!({"inbox": slug})).await;
        assert!(out.to_string().contains("slug"), "{slug}: {out}");
    }
}

#[test]
fn remote_inbox_names_come_from_the_client_product() {
    use herdr_mcp::tools::mail::remote_inbox;
    assert_eq!(remote_inbox(Some("openai-mcp"), None).unwrap(), "chatgpt");
    assert_eq!(
        remote_inbox(Some("Anthropic/ClaudeAI"), None).unwrap(),
        "claude"
    );
    assert_eq!(
        remote_inbox(Some("Cursor IDE 2.0"), None).unwrap(),
        "cursor-ide-2-0"
    );
    assert_eq!(remote_inbox(None, None).unwrap(), "remote");
    assert_eq!(
        remote_inbox(Some("openai-mcp"), Some("kenwoo")).unwrap(),
        "kenwoo"
    );
}
