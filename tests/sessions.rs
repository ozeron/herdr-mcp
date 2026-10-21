#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fake_herdr::FakeHerdr;
use harness::{Harness, count, err, err_code, ok, process_info, requests, snapshot};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::herdr::SessionEnv;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::ServiceExt;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixListener;

/// A bridge whose home is the default session, with a second fake `alpha` next to it.
struct Rig {
    h: Harness,
    alpha: FakeHerdr,
    xdg: TempDir,
}

fn config_dir(xdg: &Path) -> PathBuf {
    xdg.join("herdr")
}

fn session_dir(xdg: &Path, name: &str) -> PathBuf {
    let dir = config_dir(xdg).join("sessions").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn session_env(xdg: &Path) -> SessionEnv {
    SessionEnv {
        xdg_config_home: Some(xdg.to_path_buf()),
        ..SessionEnv::default()
    }
}

async fn serve(
    home: FakeHerdr,
    xdg: &Path,
    mode: ToolMode,
    home_socket: Option<PathBuf>,
) -> Harness {
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: home_socket.unwrap_or_else(|| home.path().to_path_buf()),
        profile: Profile::new(None, None),
        mode,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv {
            pane_id: Some("w1:p1".into()),
            tab_id: Some("w1:t1".into()),
            workspace_id: Some("w1".into()),
        },
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
        install_env: None,
    };
    let env = session_env(xdg);
    let state = log_dir.path().join("state");
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let running = HerdrMcp::new(config)
            .with_session_env(env)
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
        fake: home,
        client,
        _log_dir: log_dir,
    }
}

async fn rig(mode: ToolMode) -> Rig {
    let xdg = tempfile::Builder::new().prefix("x").tempdir().unwrap();
    std::fs::create_dir_all(config_dir(xdg.path())).unwrap();
    let default = FakeHerdr::start_at(config_dir(xdg.path()).join("herdr.sock")).await;
    let alpha = FakeHerdr::start_at(session_dir(xdg.path(), "alpha").join("herdr.sock")).await;
    let h = serve(default, xdg.path(), mode, None).await;
    Rig { h, alpha, xdg }
}

/// A socket file nobody listens on, as a stopped session leaves behind.
fn stale_session(xdg: &Path, name: &str) {
    let socket = session_dir(xdg, name).join("herdr.sock");
    drop(UnixListener::bind(socket).unwrap());
}

fn script_list(fake: &FakeHerdr, workspace: &str) {
    let snapshot = json!({
        "version": "0.9.3", "protocol": 22, "layouts": [],
        "focused_workspace_id": workspace, "focused_tab_id": format!("{workspace}:t1"),
        "focused_pane_id": format!("{workspace}:p1"),
        "workspaces": [harness::workspace(workspace, 1, &format!("{workspace}:t1"))],
        "tabs": [harness::tab(&format!("{workspace}:t1"), workspace, 1)],
        "panes": [harness::pane(&format!("{workspace}:p1"), &format!("{workspace}:t1"), workspace, None)],
        "agents": [],
    });
    fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": snapshot}),
    );
    fake.reply("agent.list", json!({"type": "agent_list", "agents": []}));
}

fn names(sessions: &Value) -> Vec<&str> {
    sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn sessions_lists_default_and_named_with_running_and_active() {
    let r = rig(ToolMode::Read).await;
    stale_session(r.xdg.path(), "beta");

    let listed = ok(&r.h, "herdr_sessions", json!({})).await;

    assert_eq!(names(&listed), ["default", "alpha", "beta"]);
    let rows = listed["sessions"].as_array().unwrap();
    let running: Vec<_> = rows
        .iter()
        .map(|s| s["running"].as_bool().unwrap())
        .collect();
    assert_eq!(running, [true, true, false]);
    let active: Vec<_> = rows
        .iter()
        .map(|s| s["active"].as_bool().unwrap())
        .collect();
    assert_eq!(active, [true, false, false]);
    assert_eq!(
        rows[1]["socket"],
        json!(session_dir(r.xdg.path(), "alpha").join("herdr.sock"))
    );
}

#[tokio::test]
async fn sessions_lists_a_missing_default_as_not_running() {
    let xdg = tempfile::Builder::new().prefix("x").tempdir().unwrap();
    let home = FakeHerdr::start().await;
    let h = serve(home, xdg.path(), ToolMode::Read, None).await;

    let listed = ok(&h, "herdr_sessions", json!({})).await;

    assert_eq!(names(&listed), ["default", "custom"]);
    assert_eq!(listed["sessions"][0]["running"], false);
    assert_eq!(listed["sessions"][1]["running"], true);
    assert_eq!(listed["sessions"][1]["active"], true);
    assert_eq!(listed["sessions"][0]["active"], false);
}

#[tokio::test]
async fn use_session_switches_every_later_call() {
    let r = rig(ToolMode::Read).await;
    script_list(&r.alpha, "w9");

    let switched = ok(&r.h, "herdr_use_session", json!({"name": "alpha"})).await;

    assert_eq!(switched["active"], "alpha");
    assert_eq!(switched["previous"], "default");
    assert_eq!(
        switched["socket"],
        json!(session_dir(r.xdg.path(), "alpha").join("herdr.sock"))
    );
    let listed = ok(&r.h, "herdr_list", json!({})).await;
    assert_eq!(listed["session"], "alpha");
    assert_eq!(listed["workspaces"][0]["id"], "w9");
    assert_eq!(count(&r.alpha, "session.snapshot"), 1);
    assert_eq!(count(&r.h.fake, "session.snapshot"), 0);
    assert_eq!(requests(&r.alpha, "agent.list").len(), 1);
    let sessions = ok(&r.h, "herdr_sessions", json!({})).await;
    assert_eq!(sessions["sessions"][1]["active"], true);
    assert_eq!(sessions["sessions"][0]["active"], false);
}

#[tokio::test]
async fn use_session_refuses_bad_unknown_and_stopped_sessions() {
    let r = rig(ToolMode::Read).await;
    stale_session(r.xdg.path(), "beta");

    assert_eq!(
        err_code(&r.h, "herdr_use_session", json!({"name": "../x"})).await,
        "invalid_params"
    );
    assert_eq!(
        err_code(&r.h, "herdr_use_session", json!({"name": ""})).await,
        "invalid_params"
    );
    assert_eq!(
        err_code(&r.h, "herdr_use_session", json!({"name": "nope"})).await,
        "session_not_found"
    );
    assert_eq!(
        err_code(&r.h, "herdr_use_session", json!({"name": "beta"})).await,
        "session_not_running"
    );
    let still = ok(&r.h, "herdr_sessions", json!({})).await;
    assert_eq!(still["sessions"][0]["active"], true);
}

#[tokio::test]
async fn use_session_refuses_a_default_that_is_not_running() {
    let xdg = tempfile::Builder::new().prefix("x").tempdir().unwrap();
    let home = FakeHerdr::start().await;
    let h = serve(home, xdg.path(), ToolMode::Read, None).await;

    assert_eq!(
        err_code(&h, "herdr_use_session", json!({"name": "default"})).await,
        "session_not_running"
    );
}

#[tokio::test]
async fn a_custom_home_can_be_switched_back_to_by_name() {
    let xdg = tempfile::Builder::new().prefix("x").tempdir().unwrap();
    std::fs::create_dir_all(config_dir(xdg.path())).unwrap();
    let _default = FakeHerdr::start_at(config_dir(xdg.path()).join("herdr.sock")).await;
    let home = FakeHerdr::start().await;
    let h = serve(home, xdg.path(), ToolMode::Read, None).await;

    let away = ok(&h, "herdr_use_session", json!({"name": "default"})).await;
    let back = ok(&h, "herdr_use_session", json!({"name": "custom"})).await;

    assert_eq!(away["previous"], "custom");
    assert_eq!(back["previous"], "default");
    assert_eq!(back["socket"], json!(h.fake.path()));
}

#[tokio::test]
async fn outside_home_the_homes_caller_pane_is_not_self() {
    let r = rig(ToolMode::Control).await;
    r.h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
    r.h.fake.reply("session.snapshot", snapshot(None));
    r.alpha.reply("session.snapshot", snapshot(None));

    // Home: the caller's own pane is refused.
    let args = json!({"target": "w1:p1"});
    assert_eq!(
        err_code(&r.h, "herdr_close", args.clone()).await,
        "self_target"
    );

    // Away: the same pane id is just a pane of another session.
    ok(&r.h, "herdr_use_session", json!({"name": "alpha"})).await;
    assert_eq!(
        err_code(&r.h, "herdr_close", args.clone()).await,
        "not_launched_by_bridge"
    );
    assert_eq!(count(&r.alpha, "pane.process_info"), 0);
    assert_eq!(count(&r.h.fake, "pane.process_info"), 1);

    // Home again: the guard is back.
    r.h.fake.reply("session.snapshot", snapshot(None));
    ok(&r.h, "herdr_use_session", json!({"name": "default"})).await;
    assert_eq!(err_code(&r.h, "herdr_close", args).await, "self_target");
}

#[tokio::test]
async fn outside_home_mail_has_no_caller() {
    let r = rig(ToolMode::Read).await;
    r.alpha.reply("session.snapshot", snapshot(None));
    ok(&r.h, "herdr_use_session", json!({"name": "alpha"})).await;

    for (tool, args) in [
        ("herdr_mail_read", json!({})),
        ("herdr_mail_wait", json!({"timeout_ms": 10})),
    ] {
        assert_eq!(err_code(&r.h, tool, args).await, "no_caller", "{tool}");
    }
    // Sending needs no caller: outside home it goes out as `external`, as before.
    let sent = ok(
        &r.h,
        "herdr_mail_send",
        json!({"to": "w1:p2", "body": "hi"}),
    )
    .await;
    assert_eq!(sent["delivered"], true);
    assert_eq!(count(&r.alpha, "pane.process_info"), 0);
}

#[tokio::test]
async fn outside_home_list_and_read_have_no_caller() {
    let r = rig(ToolMode::Read).await;
    ok(&r.h, "herdr_use_session", json!({"name": "alpha"})).await;
    script_list(&r.alpha, "w9");
    script_list(&r.alpha, "w9");

    let listed = ok(&r.h, "herdr_list", json!({})).await;
    assert_eq!(listed["caller"], Value::Null);
    let scoped = err(&r.h, "herdr_list", json!({"scope": "tab"})).await;
    assert_eq!(scoped["code"], "no_caller");
    let read = err(&r.h, "herdr_read", json!({})).await;
    assert_eq!(read["code"], "no_target");
}

#[tokio::test]
async fn capabilities_and_list_show_the_session() {
    let r = rig(ToolMode::Read).await;
    for fake in [&r.h.fake, &r.alpha] {
        fake.reply(
            "ping",
            json!({"type": "pong", "version": "0.9.3", "protocol": 22}),
        );
        fake.reply(
            "server.agent_manifests",
            json!({"type": "agent_manifest_status", "last_check_unix": null, "last_result": null, "manifests": []}),
        );
    }
    r.h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
    script_list(&r.h.fake, "w1");
    r.h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );

    let home = ok(&r.h, "herdr_capabilities", json!({})).await;
    assert_eq!(
        home["session"],
        json!({
            "name": "default",
            "socket": config_dir(r.xdg.path()).join("herdr.sock"),
            "home": true,
        })
    );
    assert_eq!(home["caller_verified"], true);
    assert_eq!(
        ok(&r.h, "herdr_list", json!({})).await["session"],
        "default"
    );

    ok(&r.h, "herdr_use_session", json!({"name": "alpha"})).await;
    let away = ok(&r.h, "herdr_capabilities", json!({})).await;
    assert_eq!(away["session"]["name"], "alpha");
    assert_eq!(away["session"]["home"], false);
    assert_eq!(away["caller"], Value::Null);
    assert_eq!(away["caller_verified"], false);
    assert_eq!(away["guard_target"], Value::Null);
}

#[tokio::test]
async fn both_modes_register_the_session_tools() {
    for mode in [ToolMode::Read, ToolMode::Control] {
        let r = rig(mode).await;
        let tools = r.h.client.list_all_tools().await.unwrap();
        let tool = |name: &str| {
            tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} is not registered"))
                .clone()
        };
        let sessions = tool("herdr_sessions");
        assert_eq!(sessions.annotations.unwrap().read_only_hint, Some(true));
        let using = tool("herdr_use_session");
        let hints = using.annotations.unwrap();
        assert_eq!(hints.read_only_hint, Some(false));
        assert_eq!(hints.destructive_hint, Some(false));
        assert_eq!(using.input_schema["required"], json!(["name"]));
    }
}
