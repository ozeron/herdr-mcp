#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_herdr::{FakeHerdr, Script};
use harness::{
    Harness, PARENT, count, err, err_code, ok, ok_reply, pane, requests, snapshot, tab, workspace,
};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use serde_json::{Value, json};

struct Opts {
    env_pane: Option<&'static str>,
    max_wait: Option<Duration>,
    /// Serve as a tunnel bridge: a remote caller with no pane.
    remote: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env_pane: Some("w1:p1"),
            max_wait: None,
            remote: false,
        }
    }
}

struct NoStop;

impl herdr_mcp::tunnel::Stopper for NoStop {
    fn stop(&self) {}
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
        },
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
        install_env: None,
    };
    let tunnel = opts.remote.then(|| herdr_mcp::tunnel::TunnelConfig {
        state_home: log_dir.path().join("state"),
        expires_at: None,
        stopper: Arc::new(NoStop),
        start_command: "herdr-mcp tunnel start",
    });
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let mut server = HerdrMcp::new(config);
        if let Some(tunnel) = tunnel {
            server = server.with_tunnel(tunnel);
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
    Harness {
        fake,
        client,
        _log_dir: log_dir,
    }
}

/// w1:t1 holds the caller w1:p1 and a human pane w1:p2.
/// w1:t2 holds w1:p3 (launched by the caller) and w1:p4 (the agent "rev", launched by the caller).
/// w1:t3 holds w1:p5 (launched by the caller) alone. w2:t1 holds w2:p1 alone.
fn process_info(pane_id: &str, shell: u32, fg: u32) -> Value {
    harness::process_info_named(pane_id, shell, fg, "node", "node")
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
    assert!(out.get("rewait").is_none());
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
    assert_eq!(out["rewait"]["target"], "w1:p9");
    assert_eq!(out["rewait"]["output"]["match"], "listening on");
    assert_eq!(out["rewait"]["output"]["after_command"], "serve");
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
    h.fake.reply("session.snapshot", snapshot(None));
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

/// A snapshot whose w2 is a worktree at /repo/w2, focused on w1, answered on every call.
fn script_workspaces(fake: &FakeHerdr) {
    let mut snap = snapshot(None);
    snap["snapshot"]["workspaces"][1]["label"] = json!("kenwoo");
    snap["snapshot"]["workspaces"][1]["worktree"] = json!({
        "repo_key": "k", "repo_name": "repo", "repo_root": "/repo",
        "checkout_path": "/repo/w2", "is_linked_worktree": true,
    });
    fake.on_params("session.snapshot", json!({}), Script::Reply(snap));
}

fn script_external_tab(fake: &FakeHerdr, ws: &str) {
    script_workspaces(fake);
    fake.reply_seq(
        "pane.process_info",
        vec![process_info(&format!("{ws}:p9"), 31337, 31337)],
    );
    fake.reply(
        "tab.create",
        tab_created(&format!("{ws}:t9"), ws, &format!("{ws}:p9")),
    );
    fake.reply("pane.report_metadata", ok_reply());
    fake.reply("pane.send_input", ok_reply());
    reads(fake, &["$ "]);
}

fn external() -> Opts {
    Opts {
        env_pane: None,
        ..Opts::default()
    }
}

fn remote() -> Opts {
    Opts {
        env_pane: None,
        remote: true,
        ..Opts::default()
    }
}

#[tokio::test]
async fn a_remote_caller_without_a_workspace_is_refused_and_nothing_is_created() {
    let h = start(remote()).await;
    script_external_tab(&h.fake, "w2");

    let out = err(&h, "herdr_run", json!({"command": "ls"})).await;

    assert_eq!(out["code"], "destination_required");
    assert!(out["message"].as_str().unwrap().contains("remote"), "{out}");
    assert_eq!(out["workspaces"][1]["label"], "kenwoo");
    assert_eq!(count(&h.fake, "tab.create"), 0);
}

#[tokio::test]
async fn a_remote_caller_is_never_a_pane_even_with_a_pane_in_its_ancestry() {
    // The ancestry (4242) matches w1:p1's shell, and the env names it: a local bridge would
    // verify it. A tunnel bridge must not.
    let h = start(Opts {
        remote: true,
        ..Opts::default()
    })
    .await;
    script_caller(&h.fake);
    script_external_tab(&h.fake, "w2");

    let out = err(&h, "herdr_run", json!({"command": "ls"})).await;

    assert_eq!(out["code"], "destination_required");
    assert_eq!(count(&h.fake, "tab.create"), 0);
}

#[tokio::test]
async fn a_local_caller_with_no_verified_pane_keeps_the_focused_workspace_with_a_note() {
    let h = start(external()).await;
    script_external_tab(&h.fake, "w1");

    let out = ok(&h, "herdr_run", json!({"command": "ls"})).await;

    assert_eq!(
        requests(&h.fake, "tab.create")[0]["params"]["workspace_id"],
        "w1"
    );
    let note = out["warning"].as_str().unwrap();
    assert!(note.contains("focused workspace w1"), "{note}");
}

#[tokio::test]
async fn an_explicit_workspace_wins_over_focus_and_keeps_external_provenance() {
    let h = start(remote()).await;
    script_external_tab(&h.fake, "w2");

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"workspace_id": "w2"}}),
    )
    .await;

    assert_eq!(
        requests(&h.fake, "tab.create")[0]["params"]["workspace_id"],
        "w2"
    );
    assert_eq!(out["workspace"], "w2");
    assert!(out.get("warning").is_none(), "{out}");
    let meta = &requests(&h.fake, "pane.report_metadata")[0]["params"];
    assert_eq!(meta["tokens"][PARENT], "external");
}

#[tokio::test]
async fn a_workspace_label_resolves_to_its_id() {
    let h = start(external()).await;
    script_external_tab(&h.fake, "w2");

    ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"workspace_id": "KenWoo"}}),
    )
    .await;

    assert_eq!(
        requests(&h.fake, "tab.create")[0]["params"]["workspace_id"],
        "w2"
    );
}

#[tokio::test]
async fn an_explicit_workspace_wins_over_the_callers_own() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    script_external_tab(&h.fake, "w2");

    ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"tab": true, "workspace_id": "w2"}}),
    )
    .await;

    assert_eq!(
        requests(&h.fake, "tab.create")[0]["params"]["workspace_id"],
        "w2"
    );
}

#[tokio::test]
async fn an_unknown_workspace_is_refused_before_anything_is_created() {
    let h = start(external()).await;
    script_external_tab(&h.fake, "w2");

    let out = err(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"workspace_id": "w99"}}),
    )
    .await;

    assert_eq!(out["code"], "target_not_found");
    assert_eq!(count(&h.fake, "tab.create"), 0);
}

#[tokio::test]
async fn a_cwd_outside_the_destination_worktree_opens_with_a_warning() {
    let h = start(external()).await;
    script_external_tab(&h.fake, "w2");

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "cwd": "/elsewhere", "where": {"workspace_id": "w2"}}),
    )
    .await;

    assert_eq!(
        requests(&h.fake, "tab.create")[0]["params"]["cwd"],
        "/elsewhere"
    );
    let warning = out["warning"].as_str().unwrap();
    assert!(warning.contains("/repo/w2"), "{warning}");
}

#[tokio::test]
async fn a_cwd_inside_the_destination_worktree_has_no_warning() {
    let h = start(external()).await;
    script_external_tab(&h.fake, "w2");

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "cwd": "/repo/w2/src", "where": {"workspace_id": "w2"}}),
    )
    .await;

    assert!(out.get("warning").is_none(), "{out}");
}

#[tokio::test]
async fn workspace_id_with_split_or_a_new_workspace_is_invalid() {
    let h = start(Opts::default()).await;
    for placement in [
        json!({"split": "right", "of": "w1:p2", "workspace_id": "w2"}),
        json!({"workspace": true, "workspace_id": "w2"}),
    ] {
        let out = err(
            &h,
            "herdr_run",
            json!({"command": "ls", "where": placement}),
        )
        .await;
        assert!(
            out.to_string().contains("workspace_id places a new tab"),
            "{out}"
        );
    }
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
async fn an_agent_only_launch_token_does_not_let_close_skip_force() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    let mut snap = snapshot(None);
    snap["snapshot"]["panes"][1]["tokens"] = json!({ harness::AGENT_PARENT: "w1:p1" });
    h.fake.reply_seq("session.snapshot", vec![snap]);
    h.fake.reply("pane.close", ok_reply());

    let code = err_code(&h, "herdr_close", json!({"target": "w1:p2"})).await;
    assert_eq!(code, "not_launched_by_bridge");
    assert_eq!(count(&h.fake, "pane.close"), 0);

    let out = ok(&h, "herdr_close", json!({"target": "w1:p2", "force": true})).await;
    assert_eq!(out["closed"], true);
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

async fn external_split(of: Option<&str>) -> Harness {
    let h = start(Opts {
        env_pane: None,
        ..Opts::default()
    })
    .await;
    h.fake.reply("session.snapshot", snapshot(None));
    h.fake
        .reply("pane.split", pane_info("w1:p9", "w1:t1", "w1"));
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.reply("pane.send_input", ok_reply());
    h.fake.reply_seq(
        "pane.process_info",
        vec![process_info("w1:p9", 31337, 31337)],
    );
    reads(&h.fake, &["$ "]);
    let mut placement = json!({"split": "down"});
    if let Some(of) = of {
        placement["of"] = json!(of);
    }
    ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": placement}),
    )
    .await;
    h
}

#[tokio::test]
async fn split_of_a_pane_id_targets_that_pane() {
    let h = external_split(Some("w3J:p1")).await;

    let split = &requests(&h.fake, "pane.split")[0]["params"];
    assert_eq!(split["target_pane_id"], "w3J:p1");
    assert_eq!(count(&h.fake, "agent.get"), 0);
}

#[tokio::test]
async fn split_of_an_agent_name_resolves_it_through_agent_get() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    h.fake.reply(
        "agent.get",
        json!({"type": "agent_info", "agent": {
            "terminal_id": "t-w1:p4", "name": "rev", "agent": "codex", "agent_status": "idle",
            "pane_id": "w1:p4", "tab_id": "w1:t2", "workspace_id": "w1", "focused": false,
            "revision": 1, "state_change_seq": 3, "interactive_ready": true,
            "launch_pending": false}}),
    );
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    reads(&h.fake, &["$ "]);

    ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"split": "right", "of": "rev"}}),
    )
    .await;

    assert_eq!(requests(&h.fake, "agent.get")[0]["params"]["target"], "rev");
    let split = &requests(&h.fake, "pane.split")[0]["params"];
    assert_eq!(split["target_pane_id"], "w1:p4");
}

#[tokio::test]
async fn an_external_split_without_of_is_refused_before_any_mutation() {
    let h = start(Opts {
        env_pane: None,
        ..Opts::default()
    })
    .await;
    h.fake.reply("session.snapshot", snapshot(None));
    h.fake
        .reply("pane.split", pane_info("w1:p9", "w1:t1", "w1"));

    let out = err(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"split": "down"}}),
    )
    .await;

    assert_eq!(out["code"], "invalid_params");
    assert!(
        out["message"].as_str().unwrap().contains("where.of"),
        "{out}"
    );
    assert_eq!(count(&h.fake, "pane.split"), 0);
    assert_eq!(count(&h.fake, "pane.report_metadata"), 0);
}

#[tokio::test]
async fn a_verified_split_without_of_targets_the_caller_pane() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    reads(&h.fake, &["$ "]);

    ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"split": "down"}}),
    )
    .await;

    let split = &requests(&h.fake, "pane.split")[0]["params"];
    assert_eq!(split["target_pane_id"], "w1:p1");
}

#[tokio::test]
async fn of_without_split_is_invalid_params() {
    let h = start(Opts::default()).await;
    for placement in [
        json!({"of": "w1:p2"}),
        json!({"tab": true, "of": "w1:p2"}),
        json!({"pane": "w1:p2", "of": "w1:p2"}),
    ] {
        let out = err(
            &h,
            "herdr_run",
            json!({"command": "ls", "where": placement}),
        )
        .await;
        assert_eq!(out["code"], "invalid_params", "{placement}");
        assert!(
            out["message"]
                .as_str()
                .unwrap()
                .contains("of only applies to split"),
            "{out}"
        );
    }
    assert!(h.fake.requests().is_empty());
}

#[tokio::test]
async fn an_unknown_key_in_where_fails_the_call_and_mutates_nothing() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    h.fake
        .reply("tab.create", tab_created("w1:t9", "w1", "w1:p9"));
    reads(&h.fake, &["$ "]);
    for placement in [
        json!({"split": "down", "off": "x"}),
        json!({"worktree": {"branch": "b", "bas": "main"}}),
    ] {
        let mut params = CallToolRequestParams::new("herdr_run");
        params.arguments = json!({"command": "ls", "where": placement})
            .as_object()
            .cloned();
        let result = h.client.call_tool(params).await;
        // rmcp reports a parameter deserialization failure as an error result, not a JSON-RPC error.
        let result = result.unwrap();
        assert_eq!(result.is_error, Some(true), "{placement}: {result:?}");
        let text = format!("{:?}", result.content);
        assert!(text.contains("unknown field"), "{placement}: {text}");
    }
    for method in [
        "pane.split",
        "tab.create",
        "workspace.create",
        "worktree.create",
    ] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn a_readiness_read_error_keeps_the_pane_and_carries_no_details() {
    let h = start(Opts::default()).await;
    script_split(&h.fake);
    h.fake.on_seq(
        "pane.read",
        vec![
            Script::Reply(read_result("w1:p9", "$ ")),
            Script::error("pane_not_found", "gone"),
        ],
    );

    let out = err(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"split": "down"},
               "ready": {"match": "done", "timeout_ms": 5000}}),
    )
    .await;

    assert_eq!(out["code"], "pane_not_found");
    assert!(out.get("phase").is_none(), "{out}");
    assert_eq!(count(&h.fake, "pane.send_input"), 1);
    for method in ["pane.close", "tab.close", "workspace.close"] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn a_failed_send_into_an_existing_pane_closes_nothing() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake.reply("session.snapshot", snapshot(None));
    h.fake.reply("pane.read", read_result("w1:p2", "$ "));
    h.fake
        .on("pane.send_input", Script::error("pane_not_found", "gone"));
    h.fake
        .reply("pane.process_info", process_info("w1:p2", 100, 100));

    let code = err_code(
        &h,
        "herdr_run",
        json!({"command": "ls", "where": {"pane": "w1:p2"}}),
    )
    .await;

    assert_eq!(code, "pane_not_found");
    for method in ["pane.close", "tab.close", "workspace.close"] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

fn move_result(from: &str, to_ws: &str, to_pane: &str) -> Value {
    json!({"type": "pane_move", "move_result": {
        "changed": true,
        "previous_pane_id": from, "previous_workspace_id": "w1", "previous_tab_id": "w1:t2",
        "pane": pane(to_pane, &format!("{to_ws}:t9"), to_ws, Some("w1:p1")),
        "target_layout": {},
    }})
}

#[tokio::test]
async fn move_puts_a_launched_pane_in_a_new_tab_of_the_named_workspace() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, Some("w1:p1"));
    h.fake
        .reply("pane.move", move_result("w1:p3", "w2", "w2:p9"));

    let out = ok(
        &h,
        "herdr_move",
        json!({"target": "w1:p3", "workspace_id": "w2", "label": "kenwoo"}),
    )
    .await;

    let params = &requests(&h.fake, "pane.move")[0]["params"];
    assert_eq!(params["pane_id"], "w1:p3");
    assert_eq!(
        params["destination"],
        json!({"type": "new_tab", "workspace_id": "w2", "label": "kenwoo"})
    );
    assert_eq!(params["focus"], false);
    assert_eq!(out["moved"], true);
    assert_eq!(out["previous"]["pane"], "w1:p3");
    assert_eq!(out["pane"], "w2:p9");
    assert_eq!(out["workspace"], "w2");
    assert!(
        out["note"].as_str().unwrap().contains("pane ID changed"),
        "{out}"
    );
    assert_eq!(count(&h.fake, "pane.close"), 0);
}

#[tokio::test]
async fn move_refuses_a_pane_without_bridge_provenance_unless_forced() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, None);
    h.fake
        .reply("pane.move", move_result("w1:p2", "w2", "w2:p9"));

    let code = err_code(
        &h,
        "herdr_move",
        json!({"target": "w1:p2", "workspace_id": "w2"}),
    )
    .await;
    assert_eq!(code, "not_launched_by_bridge");
    assert_eq!(count(&h.fake, "pane.move"), 0);

    let out = ok(
        &h,
        "herdr_move",
        json!({"target": "w1:p2", "workspace_id": "w2", "force": true}),
    )
    .await;
    assert_eq!(out["moved"], true);
}

#[tokio::test]
async fn move_refuses_the_callers_own_pane_and_an_unknown_workspace() {
    let h = start(Opts::default()).await;
    script_close(&h.fake, Some("w1:p1"));

    let own = err_code(
        &h,
        "herdr_move",
        json!({"target": "w1:p1", "workspace_id": "w2"}),
    )
    .await;
    assert_eq!(own, "self_target");
    let unknown = err_code(
        &h,
        "herdr_move",
        json!({"target": "w1:p3", "workspace_id": "w99"}),
    )
    .await;
    assert_eq!(unknown, "target_not_found");
    assert_eq!(count(&h.fake, "pane.move"), 0);
}

#[tokio::test]
async fn a_cwd_that_climbs_out_of_the_worktree_is_warned() {
    let h = start(remote()).await;
    script_external_tab(&h.fake, "w2");

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "cwd": "/repo/w2/../other", "where": {"workspace_id": "w2"}}),
    )
    .await;

    assert!(
        out["warning"].as_str().unwrap().contains("/repo/w2"),
        "{out}"
    );
}

#[tokio::test]
async fn a_failed_lookup_only_skips_the_cwd_warning_for_the_callers_own_workspace() {
    let h = start(Opts::default()).await;
    script_caller(&h.fake);
    h.fake.on_params(
        "session.snapshot",
        json!({}),
        Script::error("internal_error", "snapshot failed"),
    );
    h.fake
        .reply("tab.create", tab_created("w1:t9", "w1", "w1:p9"));
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.reply("pane.send_input", ok_reply());
    reads(&h.fake, &["$ "]);
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);

    let out = ok(
        &h,
        "herdr_run",
        json!({"command": "ls", "cwd": "/anywhere"}),
    )
    .await;

    assert_eq!(
        requests(&h.fake, "tab.create")[0]["params"]["workspace_id"],
        "w1"
    );
    assert!(out.get("warning").is_none(), "{out}");
}
