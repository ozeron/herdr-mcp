#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_herdr::{FakeHerdr, Script};
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
    max_wait: Option<Duration>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env_pane: Some("w1:p1"),
            max_wait: None,
        }
    }
}

async fn start(opts: Opts) -> Harness {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, opts.max_wait),
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
        let running = HerdrMcp::new(config).serve(server_io).await.unwrap();
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

fn process_named(pane_id: &str, shell: u32, fg: u32, name: &str) -> Value {
    let mut info = process_info(pane_id, shell, fg);
    info["process_info"]["foreground_processes"] = json!([{"pid": fg, "name": name}]);
    info
}

fn read_result(pane_id: &str, text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": pane_id, "workspace_id": "w1", "tab_id": "w1:t1", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 7, "truncated": false}})
}

fn pane_info(id: &str, tab: &str, ws: &str) -> Value {
    json!({"type": "pane_info", "pane": pane(id, tab, ws, None)})
}

fn tab_created(tab_id: &str, ws: &str, root: &str) -> Value {
    json!({"type": "tab_created", "tab": tab(tab_id, ws, 9), "root_pane": pane(root, tab_id, ws, None)})
}

fn ok_reply() -> Value {
    json!({"type": "ok"})
}

fn script_caller(fake: &FakeHerdr) {
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
}

fn script_split(fake: &FakeHerdr) {
    script_caller(fake);
    fake.reply("pane.split", pane_info("w1:p9", "w1:t1", "w1"));
    fake.reply("pane.report_metadata", ok_reply());
    fake.reply("pane.send_input", ok_reply());
}

fn reads(fake: &FakeHerdr, texts: &[&str]) {
    fake.reply_seq(
        "pane.read",
        texts.iter().map(|t| read_result("w1:p9", t)).collect(),
    );
}

#[tokio::test]
async fn run_in_a_new_split_matches_the_ready_line_and_reports_the_pgid() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    reads(
        &h.fake,
        &[
            "$ ",
            "$ npm run dev\n",
            "$ npm run dev\n> vite\n",
            "$ npm run dev\n> vite\nlistening on 3000\n",
        ],
    );
    h.fake.reply_seq(
        "pane.process_info",
        vec![
            process_info("w1:p9", 200, 200),
            process_named("w1:p9", 200, 777, "npm"),
        ],
    );

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "npm run dev", "where": {"split": "down"},
               "ready": {"match": "listening on", "timeout_ms": 5000}}),
    )
    .await;

    assert_eq!(out["pane"], "w1:p9");
    assert_eq!(out["ready_matched"], true);
    assert_eq!(out["timed_out"], false);
    assert_eq!(out["pgid"], 777);
    assert!(
        out["excerpt"]
            .as_str()
            .unwrap()
            .contains("listening on 3000")
    );
    assert_eq!(out["created"], json!({"kind": "pane", "id": "w1:p9"}));

    let split = &requests(&h.fake, "pane.split")[0]["params"];
    assert_eq!(split["target_pane_id"], "w1:p1");
    assert_eq!(split["direction"], "down");
    let meta = &requests(&h.fake, "pane.report_metadata")[0]["params"];
    assert_eq!(meta["pane_id"], "w1:p9");
    assert_eq!(meta["source"], "herdr-mcp");
    assert_eq!(meta["tokens"][PARENT], "w1:p1");
    let input = &requests(&h.fake, "pane.send_input")[0]["params"];
    assert_eq!(
        *input,
        json!({"pane_id": "w1:p9", "text": "npm run dev", "keys": ["Enter"]})
    );
    for read in requests(&h.fake, "pane.read") {
        assert_eq!(read["params"]["source"], "recent_unwrapped");
        assert_eq!(read["params"]["lines"], 1000);
    }
    assert_eq!(count(&h.fake, "pane.wait_for_output"), 0);
    assert_eq!(count(&h.fake, "pane.close"), 0);
}

#[tokio::test]
async fn a_reused_pane_with_an_old_match_only_matches_the_new_line() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake.reply("session.snapshot", snapshot(None));
    h.fake.reply("pane.send_input", ok_reply());
    h.fake.reply_seq(
        "pane.process_info",
        vec![
            process_info("w1:p2", 100, 100),
            process_info("w1:p2", 100, 100),
            process_info("w1:p2", 100, 555),
        ],
    );
    h.fake.reply_seq(
        "pane.read",
        [
            "listening on 3000\n$ ",
            "listening on 3000\n$ npm run dev\n",
            "listening on 3000\n$ npm run dev\nlistening on 4000\n",
        ]
        .iter()
        .map(|t| read_result("w1:p2", t))
        .collect(),
    );

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "npm run dev", "where": {"pane": "w1:p2"},
               "ready": {"match": "listening on", "timeout_ms": 5000}}),
    )
    .await;

    assert_eq!(out["ready_matched"], true);
    let excerpt = out["excerpt"].as_str().unwrap();
    assert!(
        excerpt.contains("4000") && !excerpt.contains("3000"),
        "{excerpt}"
    );
    assert_eq!(out["created"], Value::Null);
    for method in [
        "pane.split",
        "tab.create",
        "pane.report_metadata",
        "pane.close",
    ] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn the_echoed_command_does_not_match_a_needle_it_contains() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    reads(
        &h.fake,
        &[
            "$ ",
            "$ npm run dev\n",
            "$ npm run dev\n",
            "$ npm run dev\n> dev server up\n",
        ],
    );
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 777)]);

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "npm run dev", "where": {"split": "right"},
               "ready": {"match": "dev", "timeout_ms": 5000}}),
    )
    .await;

    assert_eq!(out["ready_matched"], true);
    assert!(
        out["excerpt"]
            .as_str()
            .unwrap()
            .ends_with("> dev server up")
    );
    assert!(count(&h.fake, "pane.read") >= 4);
}

#[tokio::test]
async fn a_ready_line_that_scrolled_past_a_small_window_still_matches() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    let window: String = (0..80).map(|n| format!("old {n}\n")).collect();
    let flood: String = (0..400).map(|n| format!("log {n}\n")).collect();
    let after = format!("{window}$ serve\nlistening on 8080\n{flood}");
    reads(&h.fake, &[&window, &after]);
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 777)]);

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "serve", "where": {"split": "down"},
               "ready": {"match": "listening on", "timeout_ms": 5000}}),
    )
    .await;

    assert_eq!(out["ready_matched"], true);
    for read in requests(&h.fake, "pane.read") {
        assert_eq!(read["params"]["lines"], 1000);
    }
}

#[tokio::test]
async fn readiness_timeout_reports_the_tail_and_leaves_the_pane_running() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    reads(&h.fake, &["$ ", "$ serve\nbuilding...\n"]);
    h.fake.reply_seq(
        "pane.process_info",
        vec![process_named("w1:p9", 200, 777, "serve")],
    );

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "serve", "where": {"split": "down"},
               "ready": {"match": "listening on", "timeout_ms": 300}}),
    )
    .await;

    assert_eq!(out["timed_out"], true);
    assert_eq!(out["ready_matched"], false);
    assert!(out["excerpt"].as_str().unwrap().contains("building..."));
    assert_eq!(out["pgid"], 777);
    for method in [
        "pane.close",
        "tab.close",
        "pane.send_keys",
        "agent.send_keys",
    ] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn the_wait_cap_bounds_a_longer_timeout() {
    let h = start(Opts {
        max_wait: Some(Duration::from_millis(300)),
        ..Opts::default()
    })
    .await;
    script_split(&h.fake);
    reads(&h.fake, &["$ ", "$ serve\n"]);
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 777)]);

    let started = Instant::now();
    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "serve", "where": {"split": "down"},
               "ready": {"match": "ready", "timeout_ms": 600000}}),
    )
    .await;

    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(out["timed_out"], true);
    assert!(out["note"].as_str().unwrap().contains("300"));
}

#[tokio::test]
async fn run_without_ready_returns_the_pgid_and_claims_nothing() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    reads(&h.fake, &["$ ", "$ cargo test\ncompiling\n"]);
    h.fake.reply_seq(
        "pane.process_info",
        vec![
            process_info("w1:p9", 200, 200),
            process_info("w1:p9", 200, 888),
        ],
    );

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "cargo test", "where": {"split": "down"}}),
    )
    .await;

    assert_eq!(out["pgid"], 888);
    assert_eq!(out["timed_out"], false);
    assert!(out.get("ready_matched").is_none());
    assert!(out["excerpt"].as_str().unwrap().contains("compiling"));
    assert!(!out.to_string().contains("success"));
}

#[tokio::test]
async fn a_busy_pane_is_refused_and_nothing_is_typed() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake.reply("session.snapshot", snapshot(None));
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p2", 100, 555)]);

    let code = err_code(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"pane": "w1:p2"}}),
    )
    .await;

    assert_eq!(code, "pane_busy");
    assert_eq!(count(&h.fake, "pane.send_input"), 0);
    assert_eq!(count(&h.fake, "pane.read"), 0);
}

#[tokio::test]
async fn running_in_the_callers_own_pane_is_refused() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake.reply("session.snapshot", snapshot(None));

    let code = err_code(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"pane": "w1:p1"}}),
    )
    .await;

    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "pane.send_input"), 0);
}

#[tokio::test]
async fn default_placement_is_a_new_tab_in_the_callers_workspace() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake
        .reply("tab.create", tab_created("w1:t9", "w1", "w1:p9"));
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.reply("pane.send_input", ok_reply());
    reads(&h.fake, &["$ ", "$ ls\n"]);
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "cwd": "/work", "env": {"A": "1"}, "label": "ls"}),
    )
    .await;

    let create = &requests(&h.fake, "tab.create")[0]["params"];
    assert_eq!(create["workspace_id"], "w1");
    assert_eq!(create["cwd"], "/work");
    assert_eq!(create["env"], json!({"A": "1"}));
    assert_eq!(create["label"], "ls");
    assert_eq!(out["created"], json!({"kind": "tab", "id": "w1:t9"}));
    assert_eq!(out["pane"], "w1:p9");
}

#[tokio::test]
async fn an_external_caller_gets_the_focused_workspace_and_external_provenance() {
    let h = start(Opts {
        env_pane: None,
        ..Opts::default()
    })
    .await;
    h.fake.reply("session.snapshot", snapshot(None));
    h.fake.reply_seq(
        "pane.process_info",
        vec![process_info("w1:p9", 31337, 31337)],
    );
    h.fake
        .reply("tab.create", tab_created("w1:t9", "w1", "w1:p9"));
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.reply("pane.send_input", ok_reply());
    reads(&h.fake, &["$ "]);

    ok(&h, "herdr_run", json!({"command": "ls"})).await;

    assert!(
        requests(&h.fake, "tab.create")[0]["params"]
            .get("workspace_id")
            .is_none()
    );
    let meta = &requests(&h.fake, "pane.report_metadata")[0]["params"];
    assert_eq!(meta["tokens"][PARENT], "external");
}

#[tokio::test]
async fn a_worktree_uses_its_root_pane_and_cleans_up_its_workspace_on_failure() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake.reply(
        "worktree.create",
        json!({"type": "worktree_created",
               "workspace": workspace("w3", 3, "w3:t1"),
               "tab": tab("w3:t1", "w3", 1),
               "root_pane": pane("w3:p1", "w3:t1", "w3", None),
               "worktree": {"path": "/wt/feat", "branch": "feat", "is_bare": false,
                            "is_detached": false, "is_prunable": false,
                            "is_linked_worktree": true, "label": "feat"}}),
    );
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.reply("workspace.close", ok_reply());
    h.fake.reply("pane.read", read_result("w3:p1", "$ "));
    h.fake
        .on("pane.send_input", Script::error("pane_not_found", "gone"));

    let code = err_code(
        &h,
        "herdr_run",
        json!({"command": "make", "where": {"worktree": {"branch": "feat", "base": "main"}}}),
    )
    .await;

    assert_eq!(code, "pane_not_found");
    assert_eq!(count(&h.fake, "tab.create"), 0);
    let create = &requests(&h.fake, "worktree.create")[0]["params"];
    assert_eq!(create["branch"], "feat");
    assert_eq!(create["base"], "main");
    let meta = &requests(&h.fake, "pane.report_metadata")[0]["params"];
    assert_eq!(meta["pane_id"], "w3:p1");
    assert_eq!(
        requests(&h.fake, "workspace.close")[0]["params"]["workspace_id"],
        "w3"
    );
    assert_eq!(count(&h.fake, "pane.close"), 0);
    assert_eq!(count(&h.fake, "tab.close"), 0);
}

#[tokio::test]
async fn a_failed_send_closes_only_the_pane_this_call_created() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake
        .reply("pane.split", pane_info("w1:p9", "w1:t1", "w1"));
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.reply("pane.close", ok_reply());
    h.fake.reply("pane.read", read_result("w1:p9", "$ "));
    h.fake
        .on("pane.send_input", Script::error("pane_not_found", "gone"));

    err_code(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"split": "down"}}),
    )
    .await;

    assert_eq!(
        requests(&h.fake, "pane.close")[0]["params"]["pane_id"],
        "w1:p9"
    );
    assert_eq!(count(&h.fake, "pane.close"), 1);
}

#[tokio::test]
async fn a_failed_provenance_tag_cleans_up_and_nothing_is_typed() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake
        .reply("tab.create", tab_created("w1:t9", "w1", "w1:p9"));
    h.fake.on(
        "pane.report_metadata",
        Script::error("invalid_metadata_token", "bad"),
    );
    h.fake.reply("tab.close", ok_reply());

    err_code(&h, "herdr_run", json!({"command": "ls"})).await;

    assert_eq!(count(&h.fake, "pane.send_input"), 0);
    assert_eq!(
        requests(&h.fake, "tab.close")[0]["params"]["tab_id"],
        "w1:t9"
    );
}

#[tokio::test]
async fn run_rejects_an_empty_command_an_empty_match_and_conflicting_where() {
    let h = start(Opts::default()).await;
    for args in [
        json!({"command": "  "}),
        json!({"command": "ls", "ready": {"match": ""}}),
        json!({"command": "ls", "where": {"split": "down", "tab": true}}),
        json!({"command": "ls", "where": {"pane": "w1:p2"}, "cwd": "/x"}),
    ] {
        assert_eq!(
            err_code(&h, "herdr_run", args.clone()).await,
            "invalid_params",
            "{args}"
        );
    }
    assert!(h.fake.requests().is_empty());
}

fn script_close(fake: &FakeHerdr, caller_tag: Option<&str>) {
    script_caller(fake);
    fake.reply_seq("session.snapshot", vec![snapshot(caller_tag)]);
}

#[tokio::test]
async fn close_refuses_the_callers_own_pane() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, Some("w1:p0"));
    let code = err_code(&h, "herdr_close", json!({"target": "w1:p1", "force": true})).await;
    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "pane.close"), 0);
}

#[tokio::test]
async fn close_refuses_a_tab_that_contains_the_caller() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    let code = err_code(&h, "herdr_close", json!({"target": "w1:t1", "force": true})).await;
    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "tab.close"), 0);
}

#[tokio::test]
async fn close_refuses_the_env_named_pane_of_an_unverified_caller() {
    let h = start(Opts {
        env_pane: Some("w1:p2"),
        ..Opts::default()
    })
    .await;
    h.fake.reply_seq("session.snapshot", vec![snapshot(None)]);
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p2"}),
        process_info("w1:p2", 31337, 31337),
    );
    let code = err_code(&h, "herdr_close", json!({"target": "w1:p2", "force": true})).await;
    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "pane.close"), 0);
}

#[tokio::test]
async fn close_refuses_the_callers_own_agent_name() {
    let h = start(Opts {
        env_pane: Some("w1:p4"),
        ..Opts::default()
    })
    .await;
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p4"}),
        process_info("w1:p4", 4242, 4242),
    );
    h.fake.reply_seq("session.snapshot", vec![snapshot(None)]);
    let code = err_code(&h, "herdr_close", json!({"target": "rev"})).await;
    assert_eq!(code, "self_target");
    assert_eq!(count(&h.fake, "pane.close"), 0);
}

#[tokio::test]
async fn allow_self_lifts_only_the_self_guard() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, Some("w1:p0"));
    h.fake.reply("pane.close", ok_reply());

    let out = ok(
        &h,
        "herdr_close",
        json!({"target": "w1:p1", "allow_self": true}),
    )
    .await;

    assert_eq!(out["closed"], true);
    assert_eq!(
        requests(&h.fake, "pane.close")[0]["params"]["pane_id"],
        "w1:p1"
    );
}

#[tokio::test]
async fn allow_self_does_not_lift_the_provenance_guard() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    let code = err_code(
        &h,
        "herdr_close",
        json!({"target": "w1:p1", "allow_self": true}),
    )
    .await;
    assert_eq!(code, "not_launched_by_bridge");
}

#[tokio::test]
async fn close_refuses_a_pane_without_bridge_provenance_unless_forced() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake.reply("pane.close", ok_reply());

    let code = err_code(&h, "herdr_close", json!({"target": "w1:p2"})).await;
    assert_eq!(code, "not_launched_by_bridge");
    assert_eq!(count(&h.fake, "pane.close"), 0);

    let out = ok(&h, "herdr_close", json!({"target": "w1:p2", "force": true})).await;
    assert_eq!(out["closed"], true);
    assert_eq!(count(&h.fake, "pane.close"), 1);
}

#[tokio::test]
async fn close_removes_a_pane_the_bridge_launched() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake.reply("pane.close", ok_reply());

    let out = ok(&h, "herdr_close", json!({"target": "w1:p3"})).await;

    assert_eq!(out["closed"], true);
    assert_eq!(out["kind"], "pane");
    assert_eq!(out["id"], "w1:p3");
    assert_eq!(
        requests(&h.fake, "pane.close")[0]["params"]["pane_id"],
        "w1:p3"
    );
}

#[tokio::test]
async fn close_resolves_an_agent_name_to_its_pane() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake.reply("pane.close", ok_reply());

    let out = ok(&h, "herdr_close", json!({"target": "rev"})).await;

    assert_eq!(out["id"], "w1:p4");
    assert_eq!(
        requests(&h.fake, "pane.close")[0]["params"]["pane_id"],
        "w1:p4"
    );
}

#[tokio::test]
async fn close_a_tab_needs_provenance_on_every_pane_in_it() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake.reply("tab.close", ok_reply());

    let out = ok(&h, "herdr_close", json!({"target": "w1:t3"})).await;
    assert_eq!(out["kind"], "tab");
    assert_eq!(
        requests(&h.fake, "tab.close")[0]["params"]["tab_id"],
        "w1:t3"
    );

    let code = err_code(&h, "herdr_close", json!({"target": "w2:t1"})).await;
    assert_eq!(code, "not_launched_by_bridge");
    assert_eq!(count(&h.fake, "tab.close"), 1);
}

#[tokio::test]
async fn closing_a_workspaces_last_pane_returns_herdrs_confirmation_required() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake.on(
        "pane.close",
        Script::error(
            "confirmation_required",
            "closing this pane would close a worktree group",
        ),
    );

    let result = err(&h, "herdr_close", json!({"target": "w1:p5"})).await;

    assert_eq!(result["code"], "confirmation_required");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("worktree group")
    );
    assert_eq!(count(&h.fake, "pane.close"), 1);
}

#[tokio::test]
async fn close_does_not_take_a_workspace_and_reports_unknown_targets() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake
        .on("agent.get", Script::error("agent_not_found", "no agent"));

    assert_eq!(
        err_code(&h, "herdr_close", json!({"target": "w2"})).await,
        "unsupported_target"
    );
    assert_eq!(
        err_code(&h, "herdr_close", json!({"target": "nobody"})).await,
        "target_not_found"
    );
    assert_eq!(
        count(&h.fake, "pane.close") + count(&h.fake, "tab.close"),
        0
    );
}

async fn run_observed(command: &str, infos: Vec<Value>) -> Value {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    reads(&h.fake, &["$ ", "$ x\n"]);
    h.fake.reply_seq("pane.process_info", infos);
    ok(
        &h,
        "herdr_run",
        json!({"command": command, "where": {"split": "down"}}),
    )
    .await
}

#[tokio::test]
async fn a_short_lived_prompt_helper_group_is_not_taken_for_the_command() {
    let out = run_observed(
        "sleep 60",
        vec![
            process_named("w1:p9", 200, 200, "zsh"),
            process_named("w1:p9", 200, 300, "gcloud"),
            process_named("w1:p9", 200, 200, "zsh"),
            process_named("w1:p9", 200, 777, "sleep"),
        ],
    )
    .await;
    assert_eq!(out["pgid"], 777);
    assert!(out.get("note").is_none());
}

#[tokio::test]
async fn env_assignments_and_paths_are_skipped_when_naming_the_command() {
    for command in ["FOO=1 npm run dev", "env A=1 B=2 /usr/bin/npm run dev"] {
        let out = run_observed(
            command,
            vec![
                process_named("w1:p9", 200, 300, "git"),
                process_named("w1:p9", 200, 600, "npm"),
            ],
        )
        .await;
        assert_eq!(out["pgid"], 600, "{command}");
    }
}

#[tokio::test]
async fn a_process_matched_by_argv_zero_names_the_command() {
    let mut info = process_named("w1:p9", 200, 600, "node");
    info["process_info"]["foreground_processes"][0]["argv"] = json!(["/opt/bin/npm", "run", "dev"]);
    let out = run_observed(
        "npm run dev",
        vec![process_named("w1:p9", 200, 300, "git"), info],
    )
    .await;
    assert_eq!(out["pgid"], 600);
}

#[tokio::test]
async fn without_a_name_match_a_group_that_stays_in_front_is_used() {
    let out = run_observed(
        "make dev",
        vec![
            process_named("w1:p9", 200, 300, "git"),
            process_named("w1:p9", 200, 900, "node"),
        ],
    )
    .await;
    assert_eq!(out["pgid"], 900);
}

#[tokio::test]
async fn only_flickering_groups_give_a_null_pgid_and_a_note() {
    let flicker = (0..60)
        .map(|n| process_named("w1:p9", 200, 1000 + n, "tmp"))
        .collect();
    let out = run_observed("make dev", flicker).await;
    assert_eq!(out["pgid"], Value::Null);
    assert!(out["note"].as_str().unwrap().contains("make"));
}
