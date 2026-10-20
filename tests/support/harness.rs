//! Helpers shared by the tool test files. Include with `#[path = "support/harness.rs"] mod harness;`
//! after `mod fake_herdr;`.
#![allow(dead_code)]

use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::fake_herdr::FakeHerdr;

pub struct Harness {
    pub fake: FakeHerdr,
    pub client: RunningService<RoleClient, ClientConfig>,
    pub _log_dir: TempDir,
}

pub async fn call(h: &Harness, name: &'static str, args: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(name);
    params.arguments = args.as_object().cloned();
    h.client.call_tool(params).await.unwrap()
}

pub async fn ok(h: &Harness, name: &'static str, args: Value) -> Value {
    let result = call(h, name, args).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.expect("structured content")
}

pub async fn err(h: &Harness, name: &'static str, args: Value) -> Value {
    let result = call(h, name, args).await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    result.structured_content.unwrap()
}

pub async fn err_code(h: &Harness, name: &'static str, args: Value) -> String {
    err(h, name, args).await["code"]
        .as_str()
        .unwrap()
        .to_string()
}

pub fn requests(fake: &FakeHerdr, method: &str) -> Vec<Value> {
    fake.requests()
        .into_iter()
        .filter(|r| r["method"] == method)
        .collect()
}

pub fn count(fake: &FakeHerdr, method: &str) -> usize {
    requests(fake, method).len()
}

pub const PARENT: &str = "herdr_mcp_parent";
pub const AGENT_PARENT: &str = "herdr_mcp_agent_parent";

pub fn pane(id: &str, tab: &str, ws: &str, parent: Option<&str>) -> Value {
    let mut pane = json!({
        "pane_id": id, "terminal_id": format!("t-{id}"), "tab_id": tab, "workspace_id": ws,
        "focused": false, "cwd": "/work", "agent_status": "unknown", "revision": 1,
    });
    if let Some(parent) = parent {
        pane["tokens"] = json!({ PARENT: parent });
    }
    pane
}

pub fn tab(id: &str, ws: &str, number: u32) -> Value {
    json!({
        "tab_id": id, "workspace_id": ws, "number": number, "label": id,
        "focused": false, "pane_count": 1, "agent_status": "idle",
    })
}

pub fn workspace(id: &str, number: u32, tab: &str) -> Value {
    json!({
        "workspace_id": id, "number": number, "label": id, "focused": number == 1,
        "pane_count": 1, "tab_count": 1, "active_tab_id": tab, "agent_status": "idle",
    })
}

pub fn agent(id: &str, tab: &str, ws: &str, name: &str) -> Value {
    json!({
        "terminal_id": format!("t-{id}"), "name": name, "agent": "codex", "agent_status": "idle",
        "pane_id": id, "tab_id": tab, "workspace_id": ws, "focused": false, "revision": 1,
        "state_change_seq": 3, "interactive_ready": true, "launch_pending": false,
    })
}

/// A process-info reply with no foreground processes listed.
pub fn process_info(pane_id: &str, shell: u32, fg: u32) -> Value {
    json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": shell, "foreground_process_group_id": fg,
        "tty": "/dev/ttys001", "foreground_processes": []}})
}

/// A process-info reply with one named foreground process.
pub fn process_info_named(pane_id: &str, shell: u32, fg: u32, name: &str, cmdline: &str) -> Value {
    json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": shell, "foreground_process_group_id": fg,
        "tty": "/dev/ttys001",
        "foreground_processes": [{"pid": fg, "name": name, "cmdline": cmdline}]}})
}

pub fn ok_reply() -> Value {
    json!({"type": "ok"})
}

/// The two-workspace snapshot used by the run, close and interrupt tests.
pub fn snapshot(caller_tag: Option<&str>) -> Value {
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
