//! Live acceptance suite (U11). Opt-in: every test is `#[ignore]` and returns early unless
//! `HERDR_MCP_LIVE=1`. It touches only the scratch session `herdr-mcp-probe`. See README.md.

mod helpers;

use std::path::PathBuf;
use std::time::Duration;

use helpers::{
    Bridge, BridgeOpts, Workspace, agent_status, gate, pane_count, read_until, snapshot,
    statuses_except, unique, wait_until_matched,
};
use serde_json::{Value, json};

const REPLY_BUDGET: Duration = Duration::from_secs(300);

/// Launches `kind` in the workspace's root pane with `wait: turn`, then re-waits while it times out.
async fn launch_and_settle(
    bridge: &Bridge,
    ws: &Workspace,
    name: &str,
    instructions: &str,
) -> Value {
    let launched = bridge
        .call(
            "herdr_launch",
            json!({
                "kind": "codex",
                "name": name,
                "instructions": instructions,
                "where": {"pane": ws.pane},
                "wait": "turn",
                "start_timeout_ms": 60000,
            }),
        )
        .await
        .ok("herdr_launch");
    assert!(launched["turn"].is_object(), "no turn token: {launched}");
    if launched["matched"] == true {
        return launched;
    }
    assert_eq!(
        launched["timed_out"], true,
        "unexpected launch result: {launched}"
    );
    wait_until_matched(bridge, json!({"turn": launched["turn"]}), REPLY_BUDGET).await;
    launched
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1 and the herdr-mcp-probe session"]
async fn ae1_list_and_read_return_tree_and_bounded_tail() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae1"));
    let marker = unique("ae1marker");
    ws.type_command(&format!("echo {marker}"));
    let bridge = Bridge::start(BridgeOpts::read()).await;
    assert_eq!(
        bridge.tool_names().await,
        [
            "herdr_capabilities",
            "herdr_list",
            "herdr_mail_read",
            "herdr_mail_send",
            "herdr_mail_wait",
            "herdr_read",
            "herdr_wait"
        ]
    );

    let listed = bridge.call("herdr_list", json!({})).await.ok("herdr_list");
    let text = listed.to_string();
    assert!(
        text.contains(&ws.id),
        "workspace {} not in tree: {text}",
        ws.id
    );
    assert!(text.contains(&ws.pane), "pane {} not in tree", ws.pane);

    let tail = read_until(&bridge, &ws.pane, &marker, Duration::from_secs(20))
        .await
        .expect("marker never appeared in the pane tail");
    let read = bridge
        .call("herdr_read", json!({"target": ws.pane, "lines": 5}))
        .await
        .ok("herdr_read");
    let lines = read["text"].as_str().unwrap().lines().count();
    assert!(lines <= 5, "tail not bounded: {lines} lines");
    assert!(tail.contains(&marker));
    bridge.close().await;
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1"]
async fn ae1_missing_session_reports_not_running_and_bridge_stays_up() {
    gate!();
    let bridge = Bridge::start(BridgeOpts {
        session: Some("herdr-mcp-nope".into()),
        ..BridgeOpts::default()
    })
    .await;
    let first = bridge.call("herdr_capabilities", json!({})).await;
    assert!(first.is_error);
    assert_eq!(first.code(), Some("herdr_not_running"), "{}", first.body);
    let second = bridge.call("herdr_capabilities", json!({})).await;
    assert_eq!(second.code(), Some("herdr_not_running"));
    bridge.close().await;
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1 and the herdr-mcp-probe session"]
async fn ae2_ready_timeout_then_wait_matches() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae2"));
    let bridge = Bridge::start(BridgeOpts {
        max_wait_ms: Some(5000),
        ..BridgeOpts::control()
    })
    .await;
    let run = bridge
        .call(
            "herdr_run",
            json!({
                "command": "sleep 8; echo server listening on 4000; sleep 30",
                "where": {"pane": ws.pane},
                "ready": {"match": "listening on", "timeout_ms": 3000},
            }),
        )
        .await
        .ok("herdr_run");
    assert_eq!(run["timed_out"], true, "{run}");
    assert_ne!(run["ready_matched"], true, "{run}");

    let rewait = run["rewait"].clone();
    assert!(rewait.is_object(), "no rewait returned: {run}");
    let matched = wait_until_matched(&bridge, rewait, Duration::from_secs(30)).await;
    assert_eq!(matched["matched"], true);
    bridge.close().await;
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1 and the herdr-mcp-probe session"]
async fn ae2_stale_output_in_reused_pane_does_not_match() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae2s"));
    let bridge = Bridge::start(BridgeOpts::control()).await;
    let first = bridge
        .call(
            "herdr_run",
            json!({"command": "echo listening on", "where": {"pane": ws.pane}}),
        )
        .await
        .ok("herdr_run");
    assert_eq!(first["pane"], ws.pane.as_str(), "{first}");
    read_until(&bridge, &ws.pane, "listening on", Duration::from_secs(10))
        .await
        .expect("first echo never showed");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let second = bridge
        .call(
            "herdr_run",
            json!({
                "command": "sleep 6; echo other output",
                "where": {"pane": ws.pane},
                "ready": {"match": "listening on", "timeout_ms": 3000},
            }),
        )
        .await
        .ok("herdr_run");
    assert_eq!(second["timed_out"], true, "stale match leaked: {second}");
    assert_ne!(
        second["ready_matched"], true,
        "stale match leaked: {second}"
    );
    bridge.close().await;
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1, the herdr-mcp-probe session and a codex login"]
async fn ae3_launch_turn_reply_and_send_followup() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae3"));
    let name = unique("ae3-child");
    let bridge = Bridge::start(BridgeOpts::control()).await;
    launch_and_settle(
        &bridge,
        &ws,
        &name,
        "Reply with exactly one word: the English name of the color of a clear daytime sky. Do not use tools.",
    )
    .await;
    let reply = read_until(&bridge, &ws.pane, "blue", REPLY_BUDGET).await;
    assert!(reply.is_some(), "reply not visible in the pane");

    let sent = bridge
        .call(
            "herdr_send",
            json!({
                "target": name,
                "prompt": "Reply with exactly one word: the English name of the color of fresh grass. Do not use tools.",
                "mode": "after_idle",
            }),
        )
        .await
        .ok("herdr_send");
    let turn = sent["turn"].clone();
    assert!(turn.is_object(), "send returned no turn: {sent}");
    let after = turn["after"].as_u64().unwrap();
    let matched = wait_until_matched(&bridge, json!({"turn": turn}), REPLY_BUDGET).await;
    assert!(
        matched["completion_seq"].as_u64().unwrap() > after,
        "completion_seq did not advance past turn.after: {matched}"
    );
    assert!(
        read_until(&bridge, &ws.pane, "green", Duration::from_secs(30))
            .await
            .is_some()
    );
    bridge.close().await;
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1, the herdr-mcp-probe session and a codex login"]
async fn ae4_interrupt_changes_only_the_target() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae4"));
    let name = unique("ae4-child");
    let bridge = Bridge::start(BridgeOpts::control()).await;
    let launched = bridge
        .call(
            "herdr_launch",
            json!({
                "kind": "codex",
                "name": name,
                "instructions": "Write a 1500-word essay on the history of lighthouses. Do not use tools. Start writing immediately.",
                "where": {"pane": ws.pane},
                "wait": "none",
                "start_timeout_ms": 60000,
            }),
        )
        .await
        .ok("herdr_launch");
    assert!(launched["turn"].is_object(), "no turn token: {launched}");
    bridge
        .call(
            "herdr_wait",
            json!({"target": ws.pane, "until": ["working"], "timeout_ms": 20000}),
        )
        .await
        .ok("herdr_wait");

    let before = snapshot();
    assert_eq!(agent_status(&before, &ws.pane).as_deref(), Some("working"));
    let interrupted = bridge
        .call("herdr_interrupt", json!({"target": ws.pane}))
        .await
        .ok("herdr_interrupt");
    bridge
        .call(
            "herdr_wait",
            json!({"target": ws.pane, "until": ["done", "idle"], "timeout_ms": 30000}),
        )
        .await
        .ok("herdr_wait");
    let after = snapshot();

    assert_eq!(pane_count(&before), pane_count(&after), "{interrupted}");
    assert_eq!(
        agent_status(&after, &ws.pane).as_deref(),
        Some("done"),
        "{interrupted}"
    );
    assert_eq!(
        statuses_except(&before, &ws.pane),
        statuses_except(&after, &ws.pane),
        "another pane changed status"
    );
    bridge.close().await;
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1 and the herdr-mcp-probe session"]
async fn ae4_interrupting_the_callers_own_pane_is_refused() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae4self"));

    // Fail-closed path: HERDR_PANE_ID names the pane but the bridge is not under its shell.
    let env_named = Bridge::start(BridgeOpts {
        env: vec![
            ("HERDR_PANE_ID".into(), ws.pane.clone()),
            ("HERDR_ENV".into(), "1".into()),
        ],
        ..BridgeOpts::control()
    })
    .await;
    let caps = env_named
        .call("herdr_capabilities", json!({}))
        .await
        .ok("herdr_capabilities");
    assert_eq!(caps["caller_verified"], false, "{caps}");
    let refused = env_named
        .call("herdr_interrupt", json!({"target": ws.pane}))
        .await;
    assert!(refused.is_error);
    assert_eq!(refused.code(), Some("self_target"), "{}", refused.body);
    env_named.close().await;

    // Verified path: run the bridge under the pane's own shell, stdio over FIFOs.
    let dir = tempfile::tempdir().unwrap();
    let bridge = Bridge::start_in_pane(&ws.pane, dir.path(), BridgeOpts::control()).await;
    let caps = bridge
        .call("herdr_capabilities", json!({}))
        .await
        .ok("herdr_capabilities");
    assert_eq!(caps["caller_verified"], true, "{caps}");
    assert_eq!(caps["caller"]["pane_id"], ws.pane.as_str(), "{caps}");
    let refused = bridge
        .call("herdr_interrupt", json!({"target": ws.pane}))
        .await;
    assert!(refused.is_error);
    assert_eq!(refused.code(), Some("self_target"), "{}", refused.body);
    bridge.close().await;
}

/// A PATH holding only the real `claude`, `codex` and `node` plus a `herdr` stub that fails, so the
/// installer's `--status` cannot reach the real `herdr integration status`.
fn install_path(dir: &std::path::Path) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("herdr");
    std::fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut dirs = vec![bin];
    for tool in ["claude", "codex", "node"] {
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join(tool))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{tool} not on PATH"));
        let parent = found.parent().unwrap().to_path_buf();
        if !dirs.contains(&parent) {
            dirs.push(parent);
        }
    }
    dirs.extend(["/usr/bin", "/bin"].map(PathBuf::from));
    std::env::join_paths(dirs).unwrap()
}

#[test]
#[ignore = "live: needs HERDR_MCP_LIVE=1 and real claude and codex on PATH"]
fn ae5a_install_status_remove_in_scratch_dirs() {
    gate!();
    use std::process::Command;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (home, claude_dir, codex_home) =
        (root.join("home"), root.join("claude"), root.join("codex"));
    for dir in [&home, &claude_dir, &codex_home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let path = install_path(&root);
    let run = |args: &[&str]| {
        let mut command = Command::new(helpers::BIN);
        command
            .arg("install")
            .args(args)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &claude_dir)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", &path)
            .stdin(std::process::Stdio::null());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(key);
            }
        }
        command.output().expect("run install")
    };
    let text = |out: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };

    let installed = run(&["--yes"]);
    assert!(installed.status.success(), "{}", text(&installed));
    let status = run(&["--status"]);
    let report = text(&status);
    assert!(report.contains("claude: ours"), "{report}");
    assert!(report.contains("codex: ours"), "{report}");

    let removed = run(&["--remove", "--yes"]);
    assert!(removed.status.success(), "{}", text(&removed));
    let after = text(&run(&["--status"]));
    assert!(after.contains("claude: absent"), "{after}");
    assert!(after.contains("codex: absent"), "{after}");
    assert!(
        !home.join(".local/bin/herdr-mcp").exists(),
        "shim left behind"
    );
}

#[tokio::test]
#[ignore = "live: needs HERDR_MCP_LIVE=1, the herdr-mcp-probe session and a codex login"]
async fn ae5b_launched_child_sees_verified_caller_through_injected_bridge() {
    gate!();
    let ws = Workspace::create(&unique("hm-ae5b"));
    let name = unique("ae5b-child");
    let bridge = Bridge::start(BridgeOpts::control()).await;
    let launched = bridge
        .call(
            "herdr_launch",
            json!({
                "kind": "codex",
                "name": name,
                "instructions": format!(
                    "Call the herdr_capabilities tool of your herdr MCP server. Then reply with exactly one line and nothing else, in the form caller=<caller.pane_id> verified=<caller_verified>. Do not run shell commands. The expected pane is not known to you; report what the tool says. Run id {name}."
                ),
                "where": {"pane": ws.pane},
                "wait": "turn",
                "start_timeout_ms": 60000,
            }),
        )
        .await
        .ok("herdr_launch");
    // "global" once the user has run `herdr-mcp install`; "injected" on a clean machine.
    assert!(
        launched["mcp"] == "injected" || launched["mcp"] == "global",
        "{launched}"
    );
    if launched["matched"] != true {
        wait_until_matched(&bridge, json!({"turn": launched["turn"]}), REPLY_BUDGET).await;
    }
    let expected = format!("caller={} verified=true", ws.pane);
    let seen = read_until(&bridge, &ws.pane, &expected, Duration::from_secs(60)).await;
    assert!(seen.is_some(), "child did not report {expected}");
    bridge.close().await;
}
