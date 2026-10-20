#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_herdr::{FakeHerdr, Script};
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
        install_env: None,
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
async fn read_defaults_to_the_caller_with_80_lines() {
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
        request["params"]["lines"], 320,
        "fetches extra rows to fill 80 meaningful lines"
    );
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
async fn read_skips_blank_lines_and_tui_rules_unless_raw() {
    let h = start(None).await;
    let screen = "reply line one\nreply line two\n\n\n\u{2500}\u{2500}\u{2500}\n\u{203a} prompt\n\n  footer\n";
    h.fake.reply_seq(
        "agent.read",
        vec![read_result("w1:p4", screen), read_result("w1:p4", screen)],
    );
    let read = ok(&h, "herdr_read", json!({"target": "rev", "lines": 3})).await;
    assert_eq!(read["text"], "reply line two\n\u{203a} prompt\n  footer");

    let raw = ok(
        &h,
        "herdr_read",
        json!({"target": "rev", "lines": 3, "raw": true}),
    )
    .await;
    assert_eq!(raw["text"], screen);
    let rows: Vec<_> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "agent.read")
        .map(|r| r["params"]["lines"].clone())
        .collect();
    assert_eq!(rows, vec![json!(12), json!(3)]);
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
