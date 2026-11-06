#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_herdr::{FakeHerdr, Script, StreamEnd};
use harness::{Harness, count, err_code, ok, process_info_named as process_info};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use serde_json::{Value, json};

async fn start(env_pane: Option<&'static str>) -> Harness {
    start_with_sessions(env_pane, false).await
}

async fn start_with_sessions(env_pane: Option<&'static str>, sessions: bool) -> Harness {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, None),
        mode: ToolMode::Read,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv {
            pane_id: env_pane.map(Into::into),
            tab_id: env_pane.map(|_| "w1:t1".into()),
            workspace_id: env_pane.map(|_| "w1".into()),
        },
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
        install_env: sessions.then(|| herdr_mcp::install::InstallEnv {
            home: log_dir.path().to_path_buf(),
            claude_config_dir: None,
            codex_home: None,
            path: Default::default(),
            exe: "herdr-mcp".into(),
        }),
    };
    let state = log_dir.path().join("state");
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let running = HerdrMcp::new(config)
            .with_mailbox_state_dir(state)
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

fn pane(id: &str, tab: &str, ws: &str, status: &str) -> Value {
    json!({
        "pane_id": id, "terminal_id": format!("t-{id}"), "tab_id": tab, "workspace_id": ws,
        "focused": false, "cwd": "/work", "agent_status": status, "revision": 1,
    })
}

fn agent(id: &str, tab: &str, ws: &str, name: &str, kind: &str, status: &str) -> Value {
    json!({
        "terminal_id": format!("t-{id}"), "name": name, "agent": kind, "agent_status": status,
        "pane_id": id, "tab_id": tab, "workspace_id": ws, "focused": false, "revision": 1,
        "state_change_seq": 3, "interactive_ready": true, "launch_pending": false,
    })
}

fn workspace(id: &str, number: u32, label: &str, tab: &str) -> Value {
    json!({
        "workspace_id": id, "number": number, "label": label, "focused": number == 1,
        "pane_count": 1, "tab_count": 1, "active_tab_id": tab, "agent_status": "idle",
    })
}

fn tab(id: &str, ws: &str, number: u32, label: &str) -> Value {
    json!({
        "tab_id": id, "workspace_id": ws, "number": number, "label": label,
        "focused": false, "pane_count": 1, "agent_status": "idle",
    })
}

fn fixture() -> (Value, Value) {
    let agents = vec![
        agent("w1:p1", "w1:t1", "w1", "me", "claude", "working"),
        {
            let mut a = agent("w1:p4", "w1:t2", "w1", "rev", "codex", "blocked");
            a["tokens"] = json!({"herdr_mcp_parent": "w1:p1"});
            a
        },
        agent("w2:p1", "w2:t1", "w2", "solo", "claude", "idle"),
    ];
    let snapshot = json!({
        "version": "0.9.3", "protocol": 22, "layouts": [],
        "focused_workspace_id": "w1", "focused_tab_id": "w1:t1", "focused_pane_id": "w1:p1",
        "workspaces": [workspace("w1", 1, "main", "w1:t1"), workspace("w2", 2, "other", "w2:t1")],
        "tabs": [
            tab("w1:t1", "w1", 1, "dev"),
            tab("w1:t2", "w1", 2, "review"),
            tab("w2:t1", "w2", 1, "solo"),
        ],
        "panes": [
            pane("w1:p1", "w1:t1", "w1", "working"),
            pane("w1:p2", "w1:t1", "w1", "unknown"),
            pane("w1:p3", "w1:t1", "w1", "unknown"),
            pane("w1:p4", "w1:t2", "w1", "blocked"),
            pane("w2:p1", "w2:t1", "w2", "idle"),
        ],
        "agents": agents,
    });
    (snapshot, json!(agents))
}

fn script_tree(fake: &FakeHerdr) {
    let (snapshot, _) = fixture();
    script_tree_with(fake, snapshot);
}

#[tokio::test]
async fn wait_change_reports_multiple_agent_changes_without_claiming_completion() {
    let h = start(None).await;
    let (snapshot, _) = fixture();
    h.fake.reply_seq(
        "session.snapshot",
        vec![json!({"type": "session_snapshot", "snapshot": snapshot})],
    );
    h.fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Hold,
        },
    );
    let mut first = agent("w1:p4", "w1:t2", "w1", "rev", "codex", "working");
    first["state_change_seq"] = json!(4);
    let mut second = agent("w2:p1", "w2:t1", "w2", "solo", "claude", "blocked");
    second["state_change_seq"] = json!(8);
    h.fake.reply_params(
        "agent.get",
        json!({"target": "w1:p4"}),
        json!({"type": "agent_info", "agent": first}),
    );
    h.fake.reply_params(
        "agent.get",
        json!({"target": "w2:p1"}),
        json!({"type": "agent_info", "agent": second}),
    );

    let out = ok(
        &h,
        "herdr_wait_change",
        json!({"targets": [
        {"target": "w1:p4", "after": 3}, {"target": "w2:p1", "after": 7}
    ], "timeout_ms": 1000}),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["changed"].as_array().unwrap().len(), 2);
    assert_eq!(out["changed"][0]["status"], "working");
    assert_eq!(out["changed"][1]["status"], "blocked");
    assert_eq!(count(&h.fake, "events.subscribe"), 1);
}

#[tokio::test]
async fn wait_change_wakes_on_an_event_and_reads_the_new_sequence() {
    let h = start(None).await;
    let (snapshot, _) = fixture();
    h.fake.reply_seq(
        "session.snapshot",
        vec![json!({"type": "session_snapshot", "snapshot": snapshot})],
    );
    h.fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![json!({"event": "pane.agent_status_changed", "data": {
                "pane_id": "w1:p4", "agent_status": "done"
            }})],
            end: StreamEnd::Hold,
        },
    );
    let mut before = agent("w1:p4", "w1:t2", "w1", "rev", "codex", "working");
    before["state_change_seq"] = json!(3);
    let mut after = before.clone();
    after["agent_status"] = json!("done");
    after["state_change_seq"] = json!(4);
    h.fake.reply_seq(
        "agent.get",
        vec![
            json!({"type": "agent_info", "agent": before}),
            json!({"type": "agent_info", "agent": after}),
        ],
    );

    let out = ok(
        &h,
        "herdr_wait_change",
        json!({"targets": [
        {"target": "w1:p4", "after": 3}
    ], "timeout_ms": 1000}),
    )
    .await;

    assert_eq!(out["changed"][0]["status"], "done");
    assert_eq!(out["changed"][0]["state_change_seq"], 4);
    assert_eq!(count(&h.fake, "agent.get"), 2);
}

#[tokio::test]
async fn wait_change_refuses_a_sequence_from_a_different_agent_lifetime() {
    let h = start(None).await;
    let (snapshot, _) = fixture();
    h.fake.reply_seq(
        "session.snapshot",
        vec![json!({"type": "session_snapshot", "snapshot": snapshot})],
    );
    h.fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Hold,
        },
    );
    let mut current = agent("w1:p4", "w1:t2", "w1", "rev", "codex", "working");
    current["state_change_seq"] = json!(2);
    h.fake.reply_params(
        "agent.get",
        json!({"target": "w1:p4"}),
        json!({"type": "agent_info", "agent": current}),
    );

    let error = harness::err(
        &h,
        "herdr_wait_change",
        json!({"targets": [
            {"target": "w1:p4", "after": 9}
        ]}),
    )
    .await;

    assert_eq!(error["code"], "stale_sequence");
}

#[tokio::test]
async fn wait_change_times_out_without_mutating_agents() {
    let h = start(None).await;
    let (snapshot, _) = fixture();
    h.fake.reply_seq(
        "session.snapshot",
        vec![json!({"type": "session_snapshot", "snapshot": snapshot})],
    );
    h.fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Hold,
        },
    );
    let mut agent = agent("w1:p4", "w1:t2", "w1", "rev", "codex", "working");
    agent["state_change_seq"] = json!(3);
    h.fake.reply_params(
        "agent.get",
        json!({"target": "w1:p4"}),
        json!({"type": "agent_info", "agent": agent}),
    );

    let out = ok(
        &h,
        "herdr_wait_change",
        json!({"targets": [
        {"target": "w1:p4", "after": 3}
    ], "timeout_ms": 30}),
    )
    .await;

    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], true);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn wait_change_rejects_own_pane_and_duplicate_targets() {
    let h = start(Some("w1:p1")).await;
    let (snapshot, _) = fixture();
    h.fake.reply_seq(
        "session.snapshot",
        vec![json!({"type": "session_snapshot", "snapshot": snapshot})],
    );
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242, "zsh", "zsh"),
    );
    let own = harness::err(
        &h,
        "herdr_wait_change",
        json!({"targets": [{"target": "w1:p1", "after": 3}]}),
    )
    .await;
    assert_eq!(own["code"], "self_target");
    let duplicate = harness::err(
        &h,
        "herdr_wait_change",
        json!({"targets": [
            {"target": "w1:p4", "after": 3}, {"target": "w1:p4", "after": 3}
        ]}),
    )
    .await;
    assert_eq!(duplicate["code"], "invalid_params");
    assert_eq!(count(&h.fake, "events.subscribe"), 0);
}

fn script_tree_with(fake: &FakeHerdr, snapshot: Value) {
    let agents = snapshot["agents"].clone();
    fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": snapshot}),
    );
    fake.reply(
        "agent.list",
        json!({"type": "agent_list", "agents": agents}),
    );
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242, "zsh", "zsh"),
    );
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p2"}),
        process_info("w1:p2", 100, 555, "node", "npm run dev"),
    );
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p3"}),
        process_info("w1:p3", 101, 101, "zsh", "zsh"),
    );
    fake.reply_params(
        "agent.explain",
        json!({"target": "w1:p4"}),
        json!({"type": "agent_explain", "explain": {
            "agent": "codex", "state": "blocked",
            "matched_rule": {"id": "codex-approval-prompt", "state": "blocked"},
            "evaluated_rules": [{"id": "codex-approval-prompt", "matched": true,
                "state": "blocked", "evidence": {"region_preview": "PRIVATE-PREVIEW"}}]}}),
    );
}

fn panes_of(list: &Value) -> Vec<&Value> {
    list["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|w| w["tabs"].as_array().unwrap())
        .flat_map(|t| t["panes"].as_array().unwrap())
        .collect()
}

fn pane_ids(list: &Value) -> Vec<&str> {
    panes_of(list)
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect()
}

fn find<'a>(list: &'a Value, id: &str) -> &'a Value {
    panes_of(list)
        .into_iter()
        .find(|p| p["id"] == id)
        .unwrap_or_else(|| panic!("pane {id} missing"))
}

fn read_result(pane_id: &str, text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": pane_id, "workspace_id": "w1", "tab_id": "w1:t1", "source": "recent",
        "format": "text", "text": text, "revision": 7, "truncated": false}})
}

#[tokio::test]
async fn list_builds_the_tree_with_kinds_and_flags_the_caller() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;

    let workspaces = list["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0]["id"], "w1");
    assert_eq!(workspaces[0]["label"], "main");
    assert_eq!(workspaces[0]["tabs"][0]["id"], "w1:t1");
    assert_eq!(workspaces[0]["tabs"][0]["label"], "dev");
    assert_eq!(list["caller"]["pane_id"], "w1:p1");
    assert_eq!(list["caller"]["verified"], true);

    let me = find(&list, "w1:p1");
    assert_eq!(me["kind"], "agent");
    assert_eq!(me["agent"], "claude");
    assert_eq!(me["name"], "me");
    assert_eq!(me["status"], "working");
    assert_eq!(me["caller"], true);

    let dev = find(&list, "w1:p2");
    assert_eq!(dev["kind"], "process");
    assert_eq!(dev["foreground"]["name"], "node");
    assert_eq!(dev["foreground"]["cmdline"], "npm run dev");
    assert!(dev.get("caller").is_none());

    let shell = find(&list, "w1:p3");
    assert_eq!(shell["kind"], "shell");
    assert_eq!(shell["cwd"], "/work");
}

#[tokio::test]
async fn list_filters_by_kind() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({"kind": "agent"})).await;
    assert_eq!(pane_ids(&list), ["w1:p1", "w1:p4", "w2:p1"]);

    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({"kind": "process"})).await;
    assert_eq!(pane_ids(&list), ["w1:p2"]);
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_scope_tab_and_workspace_follow_the_caller() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({"scope": "tab"})).await;
    assert_eq!(pane_ids(&list), ["w1:p1", "w1:p2", "w1:p3"]);

    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({"scope": "workspace"})).await;
    assert_eq!(pane_ids(&list), ["w1:p1", "w1:p2", "w1:p3", "w1:p4"]);
}

#[tokio::test]
async fn list_scope_without_a_caller_is_an_error() {
    let h = start(None).await;
    let (snapshot, agents) = fixture();
    for _ in 0..2 {
        h.fake.reply(
            "session.snapshot",
            json!({"type": "session_snapshot", "snapshot": snapshot}),
        );
    }
    h.fake.reply(
        "agent.list",
        json!({"type": "agent_list", "agents": agents}),
    );
    assert_eq!(
        err_code(&h, "herdr_list", json!({"scope": "tab"})).await,
        "no_caller"
    );
}

#[tokio::test]
async fn list_filters_by_status() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({"status": ["blocked"]})).await;
    assert_eq!(pane_ids(&list), ["w1:p4"]);
}

#[tokio::test]
async fn list_filters_by_label() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({"label_contains": "REV"})).await;
    assert_eq!(pane_ids(&list), ["w1:p4"]);
}

/// w1:p1 has a title, w1:p4 only a terminal title (padded), w2:p1 a very long title.
fn script_titles(fake: &FakeHerdr) {
    let (mut snapshot, _) = fixture();
    let agents = snapshot["agents"].as_array_mut().unwrap();
    agents[0]["title"] = json!("Fix the flaky login test");
    agents[1]["title"] = json!("  ");
    agents[1]["terminal_title_stripped"] = json!("  Review billing migration  ");
    agents[2]["title"] = json!("x".repeat(500));
    script_tree_with(fake, snapshot);
}

#[tokio::test]
async fn an_agent_shows_its_title() {
    let h = start(Some("w1:p1")).await;
    script_titles(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;
    assert_eq!(find(&list, "w1:p1")["title"], "Fix the flaky login test");
}

#[tokio::test]
async fn the_title_falls_back_to_the_stripped_terminal_title() {
    let h = start(Some("w1:p1")).await;
    script_titles(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;
    assert_eq!(find(&list, "w1:p4")["title"], "Review billing migration");
}

#[tokio::test]
async fn a_long_title_is_capped() {
    let h = start(Some("w1:p1")).await;
    script_titles(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;
    assert_eq!(find(&list, "w2:p1")["title"], "x".repeat(120));
}

#[tokio::test]
async fn an_agent_without_a_title_and_plain_panes_have_no_title_key() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;
    for id in ["w1:p1", "w1:p2", "w1:p3", "w1:p4", "w2:p1"] {
        assert!(find(&list, id).get("title").is_none(), "{id}");
    }
}

#[tokio::test]
async fn label_contains_finds_an_agent_by_a_word_in_its_title() {
    let h = start(Some("w1:p1")).await;
    script_titles(&h.fake);
    let list = ok(&h, "herdr_list", json!({"label_contains": "FLAKY"})).await;
    assert_eq!(pane_ids(&list), ["w1:p1"]);

    let h = start(Some("w1:p1")).await;
    script_titles(&h.fake);
    let list = ok(&h, "herdr_list", json!({"label_contains": "billing"})).await;
    assert_eq!(pane_ids(&list), ["w1:p4"]);
}

/// Every filter combination lists exactly what pruning the unfiltered tree by hand would.
#[tokio::test]
async fn list_filters_match_pruning_the_unfiltered_tree() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let full = ok(&h, "herdr_list", json!({})).await;

    let contains = |text: Option<&Value>, needle: &str| {
        text.and_then(Value::as_str)
            .is_some_and(|text| text.to_lowercase().contains(needle))
    };
    for kind in [None, Some("agent"), Some("process"), Some("shell")] {
        for scope in [None, Some("tab"), Some("workspace")] {
            for label in [None, Some("DEV"), Some("rev"), Some("zzz")] {
                for status in [None, Some(["unknown"]), Some(["blocked"])] {
                    let mut args = json!({});
                    if let Some(kind) = kind {
                        args["kind"] = json!(kind);
                    }
                    if let Some(scope) = scope {
                        args["scope"] = json!(scope);
                    }
                    if let Some(label) = label {
                        args["label_contains"] = json!(label);
                    }
                    if let Some(status) = status {
                        args["status"] = json!(status);
                    }
                    let h = start(Some("w1:p1")).await;
                    script_tree(&h.fake);
                    let got = ok(&h, "herdr_list", args.clone()).await;

                    let mut expected = full.clone();
                    let needle = label.map(str::to_lowercase);
                    for workspace in expected["workspaces"].as_array_mut().unwrap() {
                        let ws_id = workspace["id"].clone();
                        let ws_label = workspace["label"].clone();
                        for tab in workspace["tabs"].as_array_mut().unwrap() {
                            let tab_id = tab["id"].clone();
                            let tab_label = tab["label"].clone();
                            let in_scope = match scope {
                                Some("tab") => tab_id == "w1:t1",
                                Some("workspace") => ws_id == "w1",
                                _ => true,
                            };
                            tab["panes"].as_array_mut().unwrap().retain(|pane| {
                                in_scope
                                    && kind.is_none_or(|kind| pane["kind"] == kind)
                                    && status.is_none_or(|status| pane["status"] == status[0])
                                    && needle.as_deref().is_none_or(|needle| {
                                        contains(Some(&ws_label), needle)
                                            || contains(Some(&tab_label), needle)
                                            || contains(pane.get("label"), needle)
                                            || contains(pane.get("name"), needle)
                                    })
                            });
                        }
                        workspace["tabs"]
                            .as_array_mut()
                            .unwrap()
                            .retain(|tab| !tab["panes"].as_array().unwrap().is_empty());
                    }
                    expected["workspaces"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|workspace| !workspace["tabs"].as_array().unwrap().is_empty());
                    assert_eq!(got, expected, "filters {args}");
                    if kind == Some("agent") {
                        // Only the caller check probes; no plain pane does.
                        let probed = h
                            .fake
                            .requests()
                            .iter()
                            .filter(|r| r["method"] == "pane.process_info")
                            .filter(|r| r["params"]["pane_id"] != "w1:p1")
                            .count();
                        assert_eq!(probed, 0, "filters {args}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn list_tail_adds_each_panes_last_lines() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    h.fake.reply_params(
        "agent.read",
        json!({"target": "w2:p1"}),
        read_result("w2:p1", "one\n\ntwo\nthree\n"),
    );
    h.fake.on_params(
        "agent.read",
        json!({}),
        Script::error("agent_not_found", "no agent"),
    );
    h.fake.reply_params(
        "pane.read",
        json!({"pane_id": "w1:p2"}),
        read_result("w1:p2", "VITE ready\n"),
    );
    h.fake.on_params(
        "pane.read",
        json!({}),
        Script::error("pane_not_found", "no pane"),
    );

    let list = ok(&h, "herdr_list", json!({"tail": 2})).await;

    assert_eq!(find(&list, "w2:p1")["tail"], json!(["two", "three"]));
    assert_eq!(find(&list, "w1:p2")["tail"], json!(["VITE ready"]));
    assert!(find(&list, "w1:p3").get("tail").is_none());
    let lines: Vec<(Value, Value)> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "agent.read" && r["params"]["target"] == "w2:p1")
        .map(|r| (r["params"]["lines"].clone(), r["params"]["source"].clone()))
        .collect();
    assert_eq!(
        lines,
        [(json!(8), json!("visible"))],
        "a listing reads the screen passively, never scrolling an agent"
    );
}

#[tokio::test]
async fn list_without_tail_reads_no_pane() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);

    let list = ok(&h, "herdr_list", json!({})).await;

    assert!(panes_of(&list).iter().all(|p| p.get("tail").is_none()));
    assert!(
        h.fake
            .requests()
            .iter()
            .all(|r| r["method"] != "agent.read" && r["method"] != "pane.read")
    );
}

#[tokio::test]
async fn blocked_agent_has_a_short_reason_and_idle_agent_has_none() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;

    assert_eq!(
        find(&list, "w1:p4")["blocked_reason"],
        "codex-approval-prompt"
    );
    assert!(find(&list, "w2:p1").get("blocked_reason").is_none());
    assert!(find(&list, "w1:p1").get("blocked_reason").is_none());
    assert_eq!(count(&h.fake, "agent.explain"), 1);
    assert!(!list.to_string().contains("PRIVATE-PREVIEW"));
}

#[tokio::test]
async fn list_surfaces_provenance_as_launched_by() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;
    assert_eq!(find(&list, "w1:p4")["launched_by"], "w1:p1");
    assert!(find(&list, "w1:p1").get("launched_by").is_none());
}

#[tokio::test]
async fn list_surfaces_provenance_on_a_non_agent_pane() {
    let h = start(Some("w1:p1")).await;
    let (mut snapshot, _) = fixture();
    snapshot["panes"][2]["tokens"] = json!({"herdr_mcp_parent": "external"});
    script_tree_with(&h.fake, snapshot);
    let list = ok(&h, "herdr_list", json!({})).await;
    assert_eq!(find(&list, "w1:p3")["launched_by"], "external");
}

#[tokio::test]
async fn list_uses_one_snapshot_one_agent_list_and_concurrent_process_info() {
    let h = start(Some("w1:p1")).await;
    for (id, shell, fg, name) in [("w1:p2", 100, 555, "node"), ("w1:p3", 101, 101, "zsh")] {
        h.fake.on_params(
            "pane.process_info",
            json!({"pane_id": id}),
            Script::Delayed(
                Duration::from_millis(400),
                Box::new(Script::Reply(process_info(id, shell, fg, name, name))),
            ),
        );
    }
    script_tree(&h.fake);
    let started = Instant::now();
    ok(&h, "herdr_list", json!({})).await;
    let elapsed = started.elapsed();

    assert_eq!(count(&h.fake, "session.snapshot"), 1);
    assert_eq!(count(&h.fake, "agent.list"), 1);
    assert_eq!(count(&h.fake, "worktree.list"), 0);
    // Agent panes are classified from agent.list; only the two others need a probe,
    // plus one for caller verification.
    assert_eq!(count(&h.fake, "pane.process_info"), 3);
    assert!(
        elapsed < Duration::from_millis(700),
        "process_info calls ran serially: {elapsed:?}"
    );
}

#[tokio::test]
async fn list_reports_herdr_not_running() {
    let h = start(Some("w1:p1")).await;
    drop(h.fake);
    // The fake's socket is gone once it is dropped.
    let result = h
        .client
        .call_tool(CallToolRequestParams::new("herdr_list"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.unwrap()["code"],
        "herdr_not_running"
    );
}

#[tokio::test]
async fn read_defaults_to_the_caller_with_120_lines() {
    let h = start(Some("w1:p1")).await;
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242, "zsh", "zsh"),
    );
    h.fake.reply_params(
        "agent.read",
        json!({"target": "w1:p1"}),
        read_result("w1:p1", "hello"),
    );
    let read = ok(&h, "herdr_read", json!({})).await;

    assert_eq!(read["pane_id"], "w1:p1");
    assert_eq!(read["text"], "hello");
    assert_eq!(read["truncated"], false);
    let request = h
        .fake
        .requests()
        .into_iter()
        .find(|r| r["method"] == "agent.read")
        .unwrap();
    assert_eq!(
        request["params"]["lines"], 190,
        "a margin of rows for blank lines, not 4x: each row of an agent's history costs a scroll"
    );
    assert_eq!(read["more_above"], false);
    assert_eq!(request["params"]["source"], "recent");
}

#[tokio::test]
async fn read_without_target_or_caller_is_an_error() {
    let h = start(None).await;
    h.fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": fixture().0}),
    );
    assert_eq!(err_code(&h, "herdr_read", json!({})).await, "no_target");
}

#[tokio::test]
async fn read_caps_lines_at_1000_and_marks_truncated() {
    let h = start(None).await;
    h.fake.reply("agent.read", read_result("w1:p4", "tail"));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 2000})).await;

    assert_eq!(read["truncated"], true);
    assert_eq!(read["lines"], 1000);
    assert!(read["note"].as_str().unwrap().contains("1000"));
    let request = h
        .fake
        .requests()
        .into_iter()
        .find(|r| r["method"] == "agent.read")
        .unwrap();
    assert_eq!(request["params"]["lines"], 1000);
}

fn read_more(pane_id: &str, text: &str) -> Value {
    let mut result = read_result(pane_id, text);
    result["read"]["truncated"] = json!(true);
    result
}

#[tokio::test]
async fn read_tops_up_once_when_blank_lines_leave_too_few() {
    let h = start(None).await;
    let sparse = "a\n\n\nb\n";
    let full = "x\ny\nz\na\nb\n";
    h.fake.reply_seq(
        "agent.read",
        vec![read_more("w1:p4", sparse), read_more("w1:p4", full)],
    );
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 4})).await;

    assert_eq!(read["text"], "y\nz\na\nb");
    let rows: Vec<_> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "agent.read")
        .map(|r| r["params"]["lines"].clone())
        .collect();
    assert_eq!(rows, vec![json!(16), json!(32)]);
    assert_eq!(read["truncated"], false, "nothing asked for was cut");
    assert_eq!(read["more_above"], true);
}

#[tokio::test]
async fn a_failed_top_up_keeps_the_first_read() {
    let h = start(None).await;
    h.fake.on_seq(
        "agent.read",
        vec![
            Script::Reply(read_more("w1:p4", "a\n\nb\n")),
            Script::error("herdr_timeout", "slow"),
        ],
    );
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 4})).await;

    assert_eq!(read["text"], "a\nb");
    assert_eq!(count(&h.fake, "agent.read"), 2);
}

#[tokio::test]
async fn read_makes_no_top_up_when_no_more_history_exists() {
    let h = start(None).await;
    h.fake.reply("agent.read", read_result("w1:p4", "a\n\nb\n"));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 4})).await;

    assert_eq!(read["text"], "a\nb");
    assert_eq!(count(&h.fake, "agent.read"), 1);
    assert_eq!(read["more_above"], false);
}

#[tokio::test]
async fn read_marks_more_above_without_calling_it_truncated() {
    let h = start(None).await;
    h.fake
        .reply("agent.read", read_more("w1:p4", "one\ntwo\nthree\n"));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 2})).await;

    assert_eq!(read["text"], "two\nthree");
    assert_eq!(read["truncated"], false);
    assert_eq!(read["more_above"], true);
}

#[tokio::test]
async fn read_cuts_text_over_max_chars_on_a_character_boundary() {
    let h = start(None).await;
    let text = "é".repeat(100);
    h.fake.reply("agent.read", read_result("w1:p4", &text));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "max_chars": 10})).await;

    assert_eq!(read["text"], "é".repeat(10));
    assert_eq!(read["truncated"], true);
}

#[tokio::test]
async fn read_keeps_short_text_whole() {
    let h = start(None).await;
    h.fake.reply("agent.read", read_result("w1:p4", "short"));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "max_chars": 10})).await;
    assert_eq!(read["text"], "short");
    assert_eq!(read["truncated"], false);
}

#[tokio::test]
async fn read_strips_ansi_escapes() {
    let h = start(None).await;
    h.fake.reply(
        "agent.read",
        read_result("w1:p4", "\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}ok\r\n"),
    );
    let read = ok(&h, "herdr_read", json!({"target": "rev"})).await;
    assert_eq!(read["text"], "red ok");
}

#[tokio::test]
async fn read_skips_blank_lines_and_tui_rules() {
    let h = start(None).await;
    let screen = "reply line one\nreply line two\n\n\n\u{2500}\u{2500}\u{2500}\n\u{203a} prompt\n\n  footer\n";
    h.fake.reply("agent.read", read_result("w1:p4", screen));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 3})).await;
    assert_eq!(read["text"], "reply line two\n\u{203a} prompt\n  footer");
    let rows: Vec<_> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "agent.read")
        .map(|r| r["params"]["lines"].clone())
        .collect();
    assert_eq!(rows, vec![json!(14)]);
}

#[tokio::test]
async fn read_resolves_a_moved_pane_through_its_old_alias() {
    let h = start(None).await;
    h.fake.reply_params(
        "agent.read",
        json!({"target": "w1:p9"}),
        read_result("w2:p3", "moved"),
    );
    let read = ok(&h, "herdr_read", json!({"target": "w1:p9"})).await;

    assert_eq!(read["pane_id"], "w2:p3");
    assert_eq!(read["text"], "moved");
    assert_eq!(h.fake.last_request()["params"]["target"], "w1:p9");
}

#[tokio::test]
async fn read_falls_back_to_visible_when_the_agent_is_not_idle() {
    let h = start(None).await;
    h.fake.on(
        "agent.read",
        Script::error("agent_not_idle", "use --source visible"),
    );
    h.fake.reply("agent.read", read_result("w1:p4", "screen"));
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 500})).await;

    assert_eq!(read["text"], "screen");
    assert_eq!(read["source"], "visible");
    assert_eq!(read["terminal_history_unavailable"], true);
    assert!(!read["note"].as_str().unwrap().is_empty());
    let reads: Vec<Value> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "agent.read")
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["params"]["source"], "recent");
    assert_eq!(reads[1]["params"]["source"], "visible");
}

#[tokio::test]
async fn read_a_non_agent_pane_falls_through_to_pane_read() {
    let h = start(None).await;
    h.fake
        .on("agent.read", Script::error("agent_not_found", "no agent"));
    h.fake
        .reply("pane.read", read_result("w1:p2", "VITE ready"));
    let read = ok(&h, "herdr_read", json!({"target": "w1:p2"})).await;

    assert_eq!(read["pane_id"], "w1:p2");
    assert_eq!(read["text"], "VITE ready");
    assert_eq!(h.fake.last_request()["params"]["pane_id"], "w1:p2");
}

#[tokio::test]
async fn read_unknown_target_is_target_not_found() {
    let h = start(None).await;
    h.fake
        .on("agent.read", Script::error("agent_not_found", "no agent"));
    h.fake
        .on("pane.read", Script::error("pane_not_found", "no pane"));
    assert_eq!(
        err_code(&h, "herdr_read", json!({"target": "ghost"})).await,
        "target_not_found"
    );
}

#[tokio::test]
async fn read_tool_carries_the_result_size_hint() {
    let h = start(None).await;
    let tools = h.client.list_all_tools().await.unwrap();
    let read = tools.iter().find(|t| t.name == "herdr_read").unwrap();
    let meta = serde_json::to_value(read).unwrap();
    assert!(
        meta["_meta"]["anthropic/maxResultSizeChars"]
            .as_u64()
            .unwrap()
            >= 32_000
    );
}

fn script_worktree(fake: &FakeHerdr) {
    fake.reply_params(
        "worktree.list",
        json!({"workspace_id": "w1"}),
        json!({"type": "worktree_list",
            "source": {"repo_key": "k", "repo_name": "repo", "repo_root": "/repo",
                "source_checkout_path": "/repo"},
            "worktrees": [
                {"path": "/repo", "branch": "main", "is_bare": false, "is_detached": false,
                 "is_prunable": false, "is_linked_worktree": false, "label": "repo"},
                {"path": "/repo-wt/feat", "branch": "feat/x", "is_bare": false,
                 "is_detached": false, "is_prunable": false, "is_linked_worktree": true,
                 "label": "feat"}]}),
    );
}

fn worktree_snapshot() -> Value {
    let mut snapshot = fixture().0;
    snapshot["workspaces"][0]["worktree"] = json!({
        "repo_key": "k", "repo_name": "repo", "repo_root": "/repo",
        "checkout_path": "/repo-wt/feat", "is_linked_worktree": true});
    snapshot
}

#[tokio::test]
async fn list_shows_the_branch_of_a_worktree_workspace() {
    let h = start(Some("w1:p1")).await;
    h.fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": worktree_snapshot()}),
    );
    script_tree(&h.fake);
    script_worktree(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;

    assert_eq!(list["workspaces"][0]["worktree"]["branch"], "feat/x");
    assert_eq!(find(&list, "w1:p2")["branch"], "feat/x");
    assert!(find(&list, "w2:p1").get("branch").is_none());
    let calls: Vec<Value> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "worktree.list")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["params"]["workspace_id"], "w1");
}

#[tokio::test]
async fn list_makes_no_worktree_call_without_worktree_workspaces() {
    let h = start(Some("w1:p1")).await;
    script_tree(&h.fake);
    let list = ok(&h, "herdr_list", json!({})).await;
    assert_eq!(count(&h.fake, "worktree.list"), 0);
    assert!(find(&list, "w1:p2").get("branch").is_none());
}

#[tokio::test]
async fn read_includes_status_for_an_agent_but_not_a_plain_pane() {
    let h = start(None).await;
    h.fake.reply_params(
        "agent.get",
        json!({"target": "rev"}),
        json!({"type": "agent_info",
            "agent": agent("w1:p4", "w1:t2", "w1", "rev", "codex", "blocked")}),
    );
    h.fake.reply("agent.read", read_result("w1:p4", "x"));
    let read = ok(&h, "herdr_read", json!({"target": "rev"})).await;
    assert_eq!(read["status"], "blocked");
    assert_eq!(count(&h.fake, "agent.get"), 1);

    let h = start(None).await;
    h.fake
        .on("agent.read", Script::error("agent_not_found", "no agent"));
    h.fake
        .on("agent.get", Script::error("agent_not_found", "no agent"));
    h.fake.reply("pane.read", read_result("w1:p2", "plain"));
    let read = ok(&h, "herdr_read", json!({"target": "w1:p2"})).await;
    assert_eq!(read["text"], "plain");
    assert!(read.get("status").is_none());
}

const SESSION_ID: &str = "019a1234-5678-7000-8000-0123456789ab";

fn session_agent(kind: &str) -> Value {
    let mut a = agent("w1:p4", "w1:t2", "w1", "rev", kind, "working");
    a["agent_session"] = json!({
        "source": format!("herdr:{kind}"), "agent": kind, "kind": "id", "value": SESSION_ID,
    });
    json!({"type": "agent_info", "agent": a})
}

fn write_session(h: &Harness, kind: &str, records: &[Value]) -> std::path::PathBuf {
    let path = if kind == "claude" {
        h._log_dir
            .path()
            .join(".claude/projects/project")
            .join(format!("{SESSION_ID}.jsonl"))
    } else {
        h._log_dir
            .path()
            .join(".codex/sessions/2026/11/05")
            .join(format!("rollout-{SESSION_ID}.jsonl"))
    };
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    std::fs::write(&path, text).unwrap();
    path
}

fn claude_message(text: &str) -> Value {
    json!({"type": "assistant", "sessionId": SESSION_ID, "uuid": "answer",
    "message": {"role": "assistant", "content": [
        {"type": "thinking", "thinking": "PRIVATE-THOUGHT"},
        {"type": "text", "text": text},
        {"type": "tool_use", "name": "Bash", "input": {"command": "PRIVATE-TOOL"}},
    ]}})
}

fn codex_meta() -> Value {
    json!({"type": "session_meta", "payload": {"id": SESSION_ID}})
}

fn codex_message(text: &str) -> Value {
    json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
        "phase": "final_answer", "content": [{"type": "output_text", "text": text}]}})
}

fn user_message(provider: &str, text: &str) -> Value {
    if provider == "claude" {
        json!({"type": "user", "sessionId": SESSION_ID,
            "message": {"role": "user", "content": [{"type": "text", "text": text},
                {"type": "tool_result", "content": "PRIVATE-TOOL-OUTPUT"}]}})
    } else {
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text},
                {"type": "input_image", "image_url": "PRIVATE-IMAGE"}]}})
    }
}

#[tokio::test]
async fn conversation_reads_user_and_assistant_text_without_tools_for_both_providers() {
    for provider in ["claude", "codex"] {
        let h = start_with_sessions(None, true).await;
        let mut records = Vec::new();
        if provider == "codex" {
            records.push(codex_meta());
        }
        records.push(user_message(provider, "What changed?"));
        records.push(if provider == "claude" {
            claude_message("Done")
        } else {
            codex_message("Done")
        });
        write_session(&h, provider, &records);
        h.fake.reply_seq("agent.get", vec![session_agent(provider)]);
        let out = ok(
            &h,
            "herdr_read",
            json!({"target": "rev", "source": "conversation"}),
        )
        .await;
        assert_eq!(out["text"], "user: What changed?\nassistant: Done");
        assert_eq!(out["context_complete"], false);
        assert_eq!(out["freshness"], "saved");
        assert_eq!(count(&h.fake, "agent.read"), 0);
        assert!(!out.to_string().contains("PRIVATE"));
    }
}

#[tokio::test]
async fn default_busy_read_prefers_saved_conversation_over_visible_screen() {
    let h = start_with_sessions(None, true).await;
    write_session(
        &h,
        "claude",
        &[
            user_message("claude", "Check status"),
            claude_message("Working"),
        ],
    );
    h.fake.reply_seq("agent.get", vec![session_agent("claude")]);

    let out = ok(&h, "herdr_read", json!({"target": "rev"})).await;

    assert_eq!(out["source"], "conversation");
    assert_eq!(out["text"], "user: Check status\nassistant: Working");
    assert_eq!(out["status"], "working");
    assert_eq!(count(&h.fake, "agent.get"), 2);
    assert_eq!(count(&h.fake, "agent.read"), 0);
    assert_eq!(count(&h.fake, "pane.read"), 0);
}

#[tokio::test]
async fn default_busy_read_marks_visible_fallback_when_transcript_is_missing() {
    let h = start_with_sessions(None, true).await;
    h.fake.reply_seq("agent.get", vec![session_agent("claude")]);
    h.fake
        .on("agent.read", Script::error("agent_not_idle", "busy"));
    h.fake
        .reply("agent.read", read_result("w1:p4", "visible fragment"));

    let out = ok(&h, "herdr_read", json!({"target": "rev"})).await;

    assert_eq!(out["source"], "visible");
    assert_eq!(out["terminal_history_unavailable"], true);
    assert_eq!(out["text"], "visible fragment");
}

#[tokio::test]
async fn assistant_only_cursor_cannot_skip_user_text_in_conversation_view() {
    let h = start_with_sessions(None, true).await;
    write_session(
        &h,
        "codex",
        &[
            codex_meta(),
            user_message("codex", "Prompt"),
            codex_message("Reply"),
        ],
    );
    h.fake.reply_seq("agent.get", vec![session_agent("codex")]);
    let assistant = ok(
        &h,
        "herdr_read",
        json!({"target": "rev", "source": "session"}),
    )
    .await;
    assert_eq!(assistant["text"], "Reply");
    let error = harness::err(
        &h,
        "herdr_read",
        json!({
            "target": "rev", "source": "conversation", "after": assistant["next_cursor"]
        }),
    )
    .await;
    assert_eq!(error["code"], "stale_cursor");
}

#[tokio::test]
async fn session_read_returns_claude_text_without_reading_or_typing_into_the_terminal() {
    let h = start_with_sessions(None, true).await;
    let mut other = claude_message("OTHER-SESSION");
    other["sessionId"] = json!("other");
    let mut sidechain = claude_message("SIDECHAIN");
    sidechain["isSidechain"] = json!(true);
    write_session(
        &h,
        "claude",
        &[other, sidechain, claude_message("café — Київ\nanswer")],
    );
    h.fake.reply_seq("agent.get", vec![session_agent("claude")]);
    let read = ok(
        &h,
        "herdr_read",
        json!({"target": "rev", "source": "session"}),
    )
    .await;
    assert_eq!(read["source"], "session");
    assert_eq!(read["text"], "café — Київ\nanswer");
    assert_eq!(read["provider"], "claude");
    assert_eq!(read["session_id"], SESSION_ID);
    assert_eq!(
        read["status"], "working",
        "status comes from Herdr, not the saved answer"
    );
    assert!(read["next_cursor"].is_string());
    assert_eq!(count(&h.fake, "agent.read"), 0);
    assert_eq!(count(&h.fake, "pane.read"), 0);
    assert!(h.fake.requests().iter().all(|r| r["method"] == "agent.get"));
}

#[tokio::test]
async fn session_read_returns_codex_messages_once_and_applies_existing_output_bounds() {
    let h = start_with_sessions(None, true).await;
    write_session(
        &h,
        "codex",
        &[
            codex_meta(),
            json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "duplicate"}}),
            json!({"type": "response_item", "payload": {"type": "reasoning", "summary": "PRIVATE"}}),
            codex_message("older\none\néééééééééé"),
        ],
    );
    h.fake.reply_seq("agent.get", vec![session_agent("codex")]);
    let read = ok(
        &h,
        "herdr_read",
        json!({"target": "rev", "source": "session", "lines": 2, "max_chars": 5}),
    )
    .await;
    assert_eq!(read["text"], "ééééé");
    assert_eq!(read["provider"], "codex");
    assert_eq!(read["truncated"], true);
    assert_eq!(read["more_above"], true);
}

#[tokio::test]
async fn session_cursor_reads_appended_complete_records_and_retries_partial_json() {
    use std::io::Write;
    for provider in ["claude", "codex"] {
        let h = start_with_sessions(None, true).await;
        let message = if provider == "claude" {
            claude_message
        } else {
            codex_message
        };
        let mut records = vec![message("first")];
        if provider == "codex" {
            records.insert(0, codex_meta());
        }
        let path = write_session(&h, provider, &records);
        h.fake.reply_seq("agent.get", vec![session_agent(provider)]);
        let first = ok(
            &h,
            "herdr_read",
            json!({"target": "rev", "source": "session"}),
        )
        .await;
        let encoded = format!("{}\n", message("second"));
        let split = encoded.len() / 2;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&encoded.as_bytes()[..split]).unwrap();
        let partial = ok(
            &h,
            "herdr_read",
            json!({"target": "rev", "source": "session", "after": first["next_cursor"]}),
        )
        .await;
        assert_eq!(partial["text"], "");
        assert_eq!(partial["transcript_pending"], true);
        assert_eq!(partial["next_cursor"], first["next_cursor"]);
        file.write_all(&encoded.as_bytes()[split..]).unwrap();
        let second = ok(
            &h,
            "herdr_read",
            json!({"target": "rev", "source": "session", "after": partial["next_cursor"]}),
        )
        .await;
        assert_eq!(second["text"], "second");
        let empty = ok(
            &h,
            "herdr_read",
            json!({"target": "rev", "source": "session", "after": second["next_cursor"]}),
        )
        .await;
        assert_eq!(empty["text"], "");
        assert_ne!(first["next_cursor"], second["next_cursor"]);
    }
}

#[tokio::test]
async fn session_read_fails_explicitly_without_metadata_and_does_not_scroll_as_a_fallback() {
    let h = start_with_sessions(None, true).await;
    h.fake.reply("agent.get", json!({"type":"agent_info", "agent": agent("w1:p4", "w1:t2", "w1", "rev", "codex", "done")}));
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
    assert_eq!(count(&h.fake, "agent.read"), 0);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"visible", "after":"bad"})
        )
        .await,
        "invalid_params"
    );
}

#[tokio::test]
async fn session_read_rejects_foreign_identity_and_ambiguous_files() {
    let h = start_with_sessions(None, true).await;
    let mut meta = codex_meta();
    meta["payload"]["id"] = json!("foreign");
    let path = write_session(&h, "codex", &[meta, codex_message("FOREIGN")]);
    h.fake.reply_seq("agent.get", vec![session_agent("codex")]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
    std::fs::write(
        &path,
        format!("{}\n{}\n", codex_meta(), codex_message("answer")),
    )
    .unwrap();
    let second = path
        .parent()
        .unwrap()
        .join(format!("another-{SESSION_ID}.jsonl"));
    std::fs::copy(&path, second).unwrap();
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
}

#[tokio::test]
async fn session_read_refuses_symlinks_and_path_like_session_ids() {
    let h = start_with_sessions(None, true).await;
    let path = write_session(&h, "claude", &[claude_message("secret")]);
    let outside = h._log_dir.path().join("outside.jsonl");
    std::fs::rename(&path, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    h.fake.reply_seq("agent.get", vec![session_agent("claude")]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
    let mut a = session_agent("claude");
    a["agent"]["agent_session"]["value"] = json!("../../outside");
    h.fake.reply_seq("agent.get", vec![a]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
}

#[tokio::test]
async fn session_cursor_rejects_replacement_truncation_and_malformed_tokens() {
    let h = start_with_sessions(None, true).await;
    let path = write_session(&h, "codex", &[codex_meta(), codex_message("first")]);
    h.fake.reply_seq("agent.get", vec![session_agent("codex")]);
    let first = ok(
        &h,
        "herdr_read",
        json!({"target":"rev", "source":"session"}),
    )
    .await;
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session", "after":"not-a-cursor"})
        )
        .await,
        "invalid_params"
    );
    std::fs::write(&path, format!("{}\n", codex_meta())).unwrap();
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session", "after":first["next_cursor"]})
        )
        .await,
        "stale_cursor"
    );
    let replacement = path.with_extension("new");
    std::fs::write(
        &replacement,
        format!(
            "{}\n{}\n",
            codex_meta(),
            codex_message("replacement longer than first")
        ),
    )
    .unwrap();
    std::fs::rename(replacement, path).unwrap();
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session", "after":first["next_cursor"]})
        )
        .await,
        "stale_cursor"
    );
}

#[tokio::test]
async fn session_read_rechecks_the_pane_identity_before_returning_text() {
    let h = start_with_sessions(None, true).await;
    write_session(&h, "claude", &[claude_message("do not return after reuse")]);
    let before = session_agent("claude");
    let mut changed = before.clone();
    changed["agent"]["terminal_id"] = json!("replacement-terminal");
    h.fake.reply_seq("agent.get", vec![before, changed]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_changed"
    );
    assert_eq!(count(&h.fake, "agent.read"), 0);
}

#[tokio::test]
async fn session_read_takes_a_bounded_tail_of_a_large_transcript() {
    let h = start_with_sessions(None, true).await;
    write_session(
        &h,
        "codex",
        &[
            codex_meta(),
            json!({"type": "response_item", "payload": {"type":"function_call_output", "output": "x".repeat(9 * 1024 * 1024)}}),
            codex_message("last answer"),
        ],
    );
    h.fake.reply_seq("agent.get", vec![session_agent("codex")]);
    let read = ok(
        &h,
        "herdr_read",
        json!({"target":"rev", "source":"session"}),
    )
    .await;
    assert_eq!(read["text"], "last answer");
    assert_eq!(read["more_above"], true);
    assert_eq!(read["more_after"], false);
    let next = ok(
        &h,
        "herdr_read",
        json!({"target":"rev", "source":"session", "after":read["next_cursor"]}),
    )
    .await;
    assert_eq!(next["text"], "");
    assert_eq!(next["next_cursor"], read["next_cursor"]);
}

#[tokio::test]
async fn session_read_rejects_malformed_complete_records_instead_of_claiming_a_clean_result() {
    use std::io::Write;
    let h = start_with_sessions(None, true).await;
    let path = write_session(&h, "claude", &[claude_message("answer")]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"not-json\n")
        .unwrap();
    h.fake.reply_seq("agent.get", vec![session_agent("claude")]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
}

#[tokio::test]
async fn session_read_reports_an_oversized_latest_record_instead_of_an_empty_answer() {
    let h = start_with_sessions(None, true).await;
    write_session(
        &h,
        "codex",
        &[codex_meta(), codex_message(&"x".repeat(9 * 1024 * 1024))],
    );
    h.fake.reply_seq("agent.get", vec![session_agent("codex")]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_unavailable"
    );
}

#[tokio::test]
async fn session_read_rejects_hard_linked_transcripts_for_both_providers() {
    for provider in ["claude", "codex"] {
        let h = start_with_sessions(None, true).await;
        let records = if provider == "claude" {
            vec![claude_message("do not expose")]
        } else {
            vec![codex_meta(), codex_message("do not expose")]
        };
        let path = write_session(&h, provider, &records);
        std::fs::hard_link(path, h._log_dir.path().join("another-name.jsonl")).unwrap();
        h.fake.reply_seq("agent.get", vec![session_agent(provider)]);
        assert_eq!(
            err_code(
                &h,
                "herdr_read",
                json!({"target":"rev", "source":"session"})
            )
            .await,
            "session_unavailable"
        );
        assert_eq!(count(&h.fake, "agent.read"), 0);
    }
}

#[tokio::test]
async fn session_read_rejects_a_provider_change_even_if_the_old_session_reference_lingers() {
    let h = start_with_sessions(None, true).await;
    write_session(&h, "claude", &[claude_message("old provider")]);
    let before = session_agent("claude");
    let mut changed = before.clone();
    changed["agent"]["agent"] = json!("codex");
    h.fake.reply_seq("agent.get", vec![before, changed]);
    assert_eq!(
        err_code(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session"})
        )
        .await,
        "session_changed"
    );
}

#[tokio::test]
async fn session_read_keeps_a_complete_record_aligned_at_the_tail_budget() {
    const RECORD_BUDGET: usize = 8 * 1024 * 1024;
    for provider in ["claude", "codex"] {
        let h = start_with_sessions(None, true).await;
        let message = if provider == "claude" {
            claude_message
        } else {
            codex_message
        };
        let framing = message("").to_string().len() + 1;
        let record = message(&"x".repeat(RECORD_BUDGET - framing));
        assert_eq!(record.to_string().len() + 1, RECORD_BUDGET);
        let prefix = if provider == "codex" {
            codex_meta()
        } else {
            json!({"type":"mode", "sessionId":SESSION_ID})
        };
        write_session(&h, provider, &[prefix, record]);
        h.fake.reply_seq("agent.get", vec![session_agent(provider)]);
        let out = ok(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session", "max_chars":5}),
        )
        .await;
        assert_eq!(out["text"], "xxxxx");
        assert_eq!(out["truncated"], true);
        let next = ok(
            &h,
            "herdr_read",
            json!({"target":"rev", "source":"session", "after":out["next_cursor"]}),
        )
        .await;
        assert_eq!(next["text"], "");
    }
}
