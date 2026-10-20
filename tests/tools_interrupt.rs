#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_herdr::FakeHerdr;
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Harness {
    fake: FakeHerdr,
    client: RunningService<RoleClient, ClientConfig>,
    _log_dir: TempDir,
}

struct Opts {
    env_pane: Option<&'static str>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env_pane: Some("w1:p1"),
        }
    }
}

async fn start(opts: Opts) -> Harness {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, None),
        mode: ToolMode::Control,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv {
            pane_id: opts.env_pane.map(Into::into),
            tab_id: opts.env_pane.map(|_| "w1:t1".into()),
            workspace_id: opts.env_pane.map(|_| "w1".into()),
            herdr_env_present: opts.env_pane.is_some(),
        },
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
    };
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let running = HerdrMcp::new(config)
            .with_interrupt_settle(Duration::from_millis(1))
            .serve(server_io)
            .await
            .unwrap();
        let _ = running.waiting().await;
    });
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("test-client", "1.0.0"),
    )
    .serve(client_io)
    .await
    .unwrap();
    Harness {
        fake,
        client,
        _log_dir: log_dir,
    }
}

async fn call(h: &Harness, name: &'static str, args: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(name);
    params.arguments = args.as_object().cloned();
    h.client.call_tool(params).await.unwrap()
}

async fn ok(h: &Harness, name: &'static str, args: Value) -> Value {
    let result = call(h, name, args).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.expect("structured content")
}

async fn err(h: &Harness, name: &'static str, args: Value) -> Value {
    let result = call(h, name, args).await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    result.structured_content.unwrap()
}

async fn err_code(h: &Harness, name: &'static str, args: Value) -> String {
    err(h, name, args).await["code"]
        .as_str()
        .unwrap()
        .to_string()
}

fn requests(fake: &FakeHerdr, method: &str) -> Vec<Value> {
    fake.requests()
        .into_iter()
        .filter(|r| r["method"] == method)
        .collect()
}

fn count(fake: &FakeHerdr, method: &str) -> usize {
    requests(fake, method).len()
}

const PARENT: &str = "herdr_mcp_parent";

fn pane(id: &str, tab: &str, ws: &str, parent: Option<&str>) -> Value {
    let mut pane = json!({
        "pane_id": id, "terminal_id": format!("t-{id}"), "tab_id": tab, "workspace_id": ws,
        "focused": false, "cwd": "/work", "agent_status": "unknown", "revision": 1,
    });
    if let Some(parent) = parent {
        pane["tokens"] = json!({ PARENT: parent });
    }
    pane
}

fn tab(id: &str, ws: &str, number: u32) -> Value {
    json!({
        "tab_id": id, "workspace_id": ws, "number": number, "label": id,
        "focused": false, "pane_count": 1, "agent_status": "idle",
    })
}

fn workspace(id: &str, number: u32, tab: &str) -> Value {
    json!({
        "workspace_id": id, "number": number, "label": id, "focused": number == 1,
        "pane_count": 1, "tab_count": 1, "active_tab_id": tab, "agent_status": "idle",
    })
}

fn agent(id: &str, tab: &str, ws: &str, name: &str) -> Value {
    json!({
        "terminal_id": format!("t-{id}"), "name": name, "agent": "codex", "agent_status": "idle",
        "pane_id": id, "tab_id": tab, "workspace_id": ws, "focused": false, "revision": 1,
        "state_change_seq": 3, "interactive_ready": true, "launch_pending": false,
    })
}

/// w1:t1 holds the caller w1:p1 and a human pane w1:p2.
/// w1:t2 holds w1:p3 (launched by the caller) and w1:p4 (the agent "rev", launched by the caller).
/// w1:t3 holds w1:p5 (launched by the caller) alone. w2:t1 holds w2:p1 alone.
fn snapshot(caller_tag: Option<&str>) -> Value {
    json!({
        "type": "session_snapshot",
        "snapshot": {
            "version": "0.9.3", "protocol": 22, "layouts": [],
            "focused_workspace_id": "w1", "focused_tab_id": "w1:t1", "focused_pane_id": "w1:p1",
            "workspaces": [workspace("w1", 1, "w1:t1"), workspace("w2", 2, "w2:t1")],
            "tabs": [tab("w1:t1", "w1", 1), tab("w1:t2", "w1", 2), tab("w1:t3", "w1", 3), tab("w2:t1", "w2", 1)],
            "panes": [
                pane("w1:p1", "w1:t1", "w1", caller_tag),
                pane("w1:p2", "w1:t1", "w1", None),
                pane("w1:p3", "w1:t2", "w1", Some("w1:p1")),
                pane("w1:p4", "w1:t2", "w1", Some("w1:p1")),
                pane("w1:p5", "w1:t3", "w1", Some("w1:p1")),
                pane("w2:p1", "w2:t1", "w2", None),
            ],
            "agents": [agent("w1:p4", "w1:t2", "w1", "rev")],
        }
    })
}

fn process_info(pane_id: &str, shell: u32, fg: u32) -> Value {
    json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": shell, "foreground_process_group_id": fg,
        "tty": "/dev/ttys001",
        "foreground_processes": [{"pid": fg, "name": "node", "cmdline": "node"}]}})
}

fn agent_info(status: &str) -> Value {
    let mut agent = agent("w1:p4", "w1:t2", "w1", "rev");
    agent["agent_status"] = json!(status);
    json!({"type": "agent_info", "agent": agent})
}

fn ok_reply() -> Value {
    json!({"type": "ok"})
}

fn script_agent(fake: &FakeHerdr, statuses: &[&str]) {
    fake.reply_seq("session.snapshot", vec![snapshot(None)]);
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
    fake.reply_seq(
        "agent.get",
        statuses.iter().map(|s| agent_info(s)).collect(),
    );
    fake.reply_seq("agent.send_keys", vec![ok_reply()]);
    fake.reply_seq("pane.send_keys", vec![ok_reply()]);
}

fn script_process(fake: &FakeHerdr, shell: u32, fg: u32) {
    fake.reply_seq("session.snapshot", vec![snapshot(None)]);
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p3"}),
        process_info("w1:p3", shell, fg),
    );
    fake.reply_seq("pane.send_keys", vec![ok_reply()]);
}

fn assert_nothing_destroyed(fake: &FakeHerdr) {
    for method in ["pane.close", "tab.close", "workspace.close", "pane.split"] {
        assert_eq!(count(fake, method), 0, "{method}");
    }
}

const TRAP: &str =
    r#"trap 'echo INT >"$1"; exit 0' INT; echo ready >"$1.ready"; while :; do sleep 0.05; done"#;

struct Spawned {
    child: Child,
    flag: std::path::PathBuf,
    _dir: TempDir,
}

impl Spawned {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn received_int(&self) -> bool {
        std::fs::read_to_string(&self.flag).is_ok_and(|s| s.trim() == "INT")
    }

    async fn wait_for_int(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.received_int() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_trapping_child() -> Spawned {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("flag");
    let child = Command::new("sh")
        .args(["-c", TRAP, "sh"])
        .arg(&flag)
        .process_group(0)
        .spawn()
        .unwrap();
    let ready = dir.path().join("flag.ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "child never became ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Spawned {
        child,
        flag,
        _dir: dir,
    }
}

#[tokio::test]
async fn auto_sends_esc_to_a_working_agent_and_reports_the_status_after() {
    let h = start(Opts::default()).await;
    script_agent(&h.fake, &["working", "done"]);

    let out = ok(&h, "herdr_interrupt", json!({"target": "rev"})).await;

    assert_eq!(out["result"], "interrupt_requested");
    assert_eq!(out["method"], "keys");
    assert_eq!(out["keys"], json!(["esc"]));
    assert_eq!(out["pane"], "w1:p4");
    assert_eq!(out["status_after"], "done");
    let sent = requests(&h.fake, "agent.send_keys");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["params"]["target"], "w1:p4");
    assert_eq!(sent[0]["params"]["keys"], json!(["esc"]));
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
    assert_eq!(count(&h.fake, "agent.get"), 2);
    assert_nothing_destroyed(&h.fake);
}

#[tokio::test]
async fn auto_sends_nothing_to_an_idle_agent() {
    for status in ["idle", "done"] {
        let h = start(Opts::default()).await;
        script_agent(&h.fake, &[status]);

        let out = ok(&h, "herdr_interrupt", json!({"target": "rev"})).await;

        assert_eq!(out["result"], "no_active_work", "{status}");
        assert_eq!(out["status_after"], status);
        assert_eq!(count(&h.fake, "agent.send_keys"), 0, "{status}");
        assert_eq!(count(&h.fake, "pane.send_keys"), 0, "{status}");
    }
}

#[tokio::test]
async fn auto_refuses_a_blocked_agent_but_explicit_keys_are_sent() {
    let h = start(Opts::default()).await;
    script_agent(&h.fake, &["blocked", "blocked", "working"]);

    let code = err_code(&h, "herdr_interrupt", json!({"target": "rev"})).await;
    assert_eq!(code, "agent_blocked");
    assert_eq!(count(&h.fake, "agent.send_keys"), 0);

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "rev", "method": "keys", "keys": ["esc"]}),
    )
    .await;
    assert_eq!(out["result"], "interrupt_requested");
    assert_eq!(count(&h.fake, "agent.send_keys"), 1);
}

#[tokio::test]
async fn auto_on_a_process_pane_sends_ctrl_c() {
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, 555);

    let out = ok(&h, "herdr_interrupt", json!({"target": "w1:p3"})).await;

    assert_eq!(out["result"], "interrupt_requested");
    assert_eq!(out["keys"], json!(["ctrl+c"]));
    assert_eq!(out["status_after"], "running");
    let sent = requests(&h.fake, "pane.send_keys");
    assert_eq!(sent[0]["params"]["pane_id"], "w1:p3");
    assert_eq!(sent[0]["params"]["keys"], json!(["ctrl+c"]));
    assert_eq!(count(&h.fake, "agent.send_keys"), 0);
}

#[tokio::test]
async fn auto_on_a_process_pane_with_the_shell_in_front_sends_nothing() {
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, 100);

    let out = ok(&h, "herdr_interrupt", json!({"target": "w1:p3"})).await;

    assert_eq!(out["result"], "no_active_work");
    assert_eq!(out["status_after"], "shell");
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
}

#[tokio::test]
async fn signal_sends_int_to_the_foreground_group_of_a_real_child() {
    let child = spawn_trapping_child().await;
    let own_group = unsafe { libc::getpgid(0) };
    assert_ne!(unsafe { libc::getpgid(child.pid() as i32) }, own_group);
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, child.pid());

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p3", "method": "signal", "signal": "INT"}),
    )
    .await;

    assert_eq!(out["result"], "interrupt_requested");
    assert_eq!(out["method"], "signal");
    assert_eq!(out["signal"], "INT");
    assert_eq!(out["pgid"], child.pid());
    assert!(child.wait_for_int().await, "child never received SIGINT");
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
}

#[tokio::test]
async fn signal_defaults_to_int() {
    let child = spawn_trapping_child().await;
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, child.pid());

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p3", "method": "signal"}),
    )
    .await;

    assert_eq!(out["signal"], "INT");
    assert!(child.wait_for_int().await);
}

#[tokio::test]
async fn signal_with_the_shell_in_front_signals_nothing() {
    let child = spawn_trapping_child().await;
    let h = start(Opts::default()).await;
    script_process(&h.fake, child.pid(), child.pid());

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p3", "method": "signal"}),
    )
    .await;

    assert_eq!(out["result"], "no_active_work");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!child.received_int(), "the shell's group was signalled");
}

#[tokio::test]
async fn signal_to_an_exited_group_is_no_active_work() {
    let mut child = spawn_trapping_child().await;
    let pgid = child.pid();
    unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
    child.child.wait().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, pgid);

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p3", "method": "signal"}),
    )
    .await;

    assert_eq!(out["result"], "no_active_work");
}

fn root_owned_group() -> Option<u32> {
    if unsafe { libc::geteuid() } == 0 {
        return None;
    }
    let ps = Command::new("ps")
        .args(["-A", "-o", "pgid=,user="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let pgid: u32 = fields.next()?.parse().ok()?;
            (fields.next()? == "root" && pgid > 1).then_some(pgid)
        })
}

#[tokio::test]
async fn signal_without_permission_is_an_error_naming_the_process() {
    let Some(pgid) = root_owned_group() else {
        eprintln!("skipped: no root-owned process group, or running as root");
        return;
    };
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, pgid);

    let err = err(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p3", "method": "signal"}),
    )
    .await;

    assert_eq!(err["code"], "signal_not_permitted");
    let message = err["message"].as_str().unwrap();
    assert!(
        message.contains("node") && message.contains(&pgid.to_string()),
        "{message}"
    );
}

#[tokio::test]
async fn signal_refuses_group_ids_that_would_hit_every_process() {
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, 1);

    let code = err_code(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p3", "method": "signal"}),
    )
    .await;

    assert_eq!(code, "invalid_pgid");
}

#[tokio::test]
async fn explicit_keys_default_to_the_provider_key() {
    let h = start(Opts::default()).await;
    script_agent(&h.fake, &["idle", "idle"]);

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "rev", "method": "keys"}),
    )
    .await;

    assert_eq!(out["result"], "interrupt_requested");
    assert_eq!(out["keys"], json!(["esc"]));
}

#[tokio::test]
async fn interrupt_refuses_the_callers_own_pane() {
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, 555);

    let code = err_code(&h, "herdr_interrupt", json!({"target": "w1:p1"})).await;

    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
}

#[tokio::test]
async fn interrupt_refuses_the_env_named_pane_of_an_unverified_caller() {
    let h = start(Opts {
        env_pane: Some("w1:p2"),
    })
    .await;
    h.fake.reply_seq("session.snapshot", vec![snapshot(None)]);
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p2"}),
        process_info("w1:p2", 31337, 31337),
    );

    let code = err_code(&h, "herdr_interrupt", json!({"target": "w1:p2"})).await;

    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
}

#[tokio::test]
async fn allow_self_lifts_the_self_guard() {
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, 555);
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );

    let out = ok(
        &h,
        "herdr_interrupt",
        json!({"target": "w1:p1", "method": "keys", "allow_self": true}),
    )
    .await;

    assert_eq!(out["result"], "interrupt_requested");
}

#[tokio::test]
async fn interrupt_refuses_a_tab_target() {
    let h = start(Opts::default()).await;
    script_process(&h.fake, 100, 555);

    let code = err_code(&h, "herdr_interrupt", json!({"target": "w1:t3"})).await;

    assert_eq!(code, "unsupported_target");
}
