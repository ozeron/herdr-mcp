#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use std::path::PathBuf;
use std::time::Duration;

use fake_herdr::{FakeHerdr, Script, StreamEnd};
use herdr_mcp::herdr::{
    AgentPromptParams, AgentStartParams, AgentStatus, ApiErrorCode, Client, HerdrError, Outcome,
    PaneReadParams, PaneSendInputParams, PaneSplitParams, ReadSource, SessionEnv, SplitDirection,
    SubscriptionEvent, TabCreateParams, WorkspaceCreateParams, WorktreeCreateParams,
    WorktreeListParams, resolve_socket_path,
};
use serde_json::{Value, json};

fn agent_json() -> Value {
    json!({
        "terminal_id": "t1",
        "name": "worker",
        "agent": "claude",
        "agent_status": "working",
        "state_labels": {},
        "tokens": {"role": "reviewer"},
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "pane_id": "w1:p1",
        "focused": false,
        "interactive_ready": true,
        "state_change_seq": 7,
        "completion_seq": 3,
        "cwd": "/tmp/x",
        "revision": 42
    })
}

fn pane_json() -> Value {
    json!({
        "pane_id": "w1:p2",
        "terminal_id": "t2",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "focused": true,
        "cwd": "/tmp/x",
        "agent_status": "idle",
        "revision": 5
    })
}

fn tab_json() -> Value {
    json!({
        "tab_id": "w1:t2", "workspace_id": "w1", "number": 2, "label": "work",
        "focused": false, "pane_count": 1, "agent_status": "unknown"
    })
}

fn workspace_json() -> Value {
    json!({
        "workspace_id": "w2", "number": 2, "label": "ws", "focused": false,
        "pane_count": 1, "tab_count": 1, "active_tab_id": "w2:t1", "agent_status": "idle"
    })
}

fn read_json() -> Value {
    json!({
        "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
        "source": "recent", "format": "text", "text": "hello\nworld",
        "revision": 9, "truncated": false
    })
}

fn pong(version: &str, protocol: u32) -> Value {
    json!({"type": "pong", "version": version, "protocol": protocol})
}

fn client(fake: &FakeHerdr) -> Client {
    Client::new(fake.path()).unwrap()
}

fn env() -> SessionEnv {
    SessionEnv {
        home: Some("/home/u".into()),
        ..SessionEnv::default()
    }
}

#[test]
fn socket_precedence_session_arg_beats_socket_path_env_beats_session_env_beats_default() {
    let full = SessionEnv {
        socket_path: Some("/run/explicit.sock".into()),
        session: Some("fromenv".into()),
        xdg_config_home: Some("/xdg".into()),
        home: Some("/home/u".into()),
    };
    assert_eq!(
        resolve_socket_path(Some("named"), &full).unwrap(),
        PathBuf::from("/xdg/herdr/sessions/named/herdr.sock")
    );
    assert_eq!(
        resolve_socket_path(None, &full).unwrap(),
        PathBuf::from("/run/explicit.sock")
    );
    let no_socket = SessionEnv {
        socket_path: None,
        ..full.clone()
    };
    assert_eq!(
        resolve_socket_path(None, &no_socket).unwrap(),
        PathBuf::from("/xdg/herdr/sessions/fromenv/herdr.sock")
    );
    let default_only = SessionEnv {
        session: None,
        ..no_socket
    };
    assert_eq!(
        resolve_socket_path(None, &default_only).unwrap(),
        PathBuf::from("/xdg/herdr/herdr.sock")
    );
}

#[test]
fn socket_default_falls_back_to_home_and_default_session_name_means_default_socket() {
    assert_eq!(
        resolve_socket_path(None, &env()).unwrap(),
        PathBuf::from("/home/u/.config/herdr/herdr.sock")
    );
    assert_eq!(
        resolve_socket_path(Some("default"), &env()).unwrap(),
        PathBuf::from("/home/u/.config/herdr/herdr.sock")
    );
}

#[test]
fn invalid_session_name_is_rejected() {
    for bad in ["", "..", "a/b", "sp ace", &"x".repeat(65)] {
        assert!(
            resolve_socket_path(Some(bad), &env()).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[tokio::test]
async fn missing_socket_file_maps_to_herdr_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(dir.path().join("nope.sock")).unwrap();
    let err = client.ping().await.unwrap_err();
    assert!(matches!(err, HerdrError::NotRunning { .. }), "{err:?}");
    assert_eq!(err.code(), "herdr_not_running");
}

#[tokio::test]
async fn stale_socket_connection_refused_maps_to_herdr_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let err = Client::new(&path).unwrap().ping().await.unwrap_err();
    assert!(matches!(err, HerdrError::NotRunning { .. }), "{err:?}");
    assert_eq!(err.code(), "herdr_not_running");
}

#[test]
fn socket_path_over_104_bytes_returns_clear_error_not_a_panic() {
    let long = PathBuf::from(format!("/tmp/{}/h.sock", "d".repeat(100)));
    assert!(long.as_os_str().len() > 104);
    let err = Client::new(&long).unwrap_err();
    assert!(
        matches!(err, HerdrError::SocketPathTooLong { len, .. } if len > 104),
        "{err:?}"
    );
    assert!(err.to_string().contains("too long"), "{err}");
}

#[tokio::test]
async fn ping_version_0_9_2_is_incompatible() {
    let fake = FakeHerdr::start().await;
    fake.reply("ping", pong("0.9.2", 21));
    let err = client(&fake).check_compat().await.unwrap_err();
    assert!(matches!(err, HerdrError::Incompatible { .. }), "{err:?}");
    assert_eq!(err.code(), "herdr_incompatible");
}

#[tokio::test]
async fn ping_version_0_9_3_with_protocol_22_is_ok() {
    let fake = FakeHerdr::start().await;
    fake.reply("ping", pong("0.9.3", 22));
    let compat = client(&fake).check_compat().await.unwrap();
    assert_eq!(compat.version, "0.9.3");
    assert_eq!(compat.protocol, 22);
}

#[tokio::test]
async fn ping_version_0_9_6_with_protocol_25_is_ok_and_protocol_reported() {
    let fake = FakeHerdr::start().await;
    fake.reply(
        "ping",
        json!({"type": "pong", "version": "0.9.6", "protocol": 25,
               "capabilities": {"live_handoff": true}}),
    );
    let compat = client(&fake).check_compat().await.unwrap();
    assert_eq!(compat.protocol, 25);
    assert_eq!(compat.capabilities.unwrap()["live_handoff"], true);
}

#[tokio::test]
async fn version_comparison_is_numeric_and_ignores_prerelease_suffix() {
    for (version, ok) in [
        ("0.10.0", true),
        ("1.0.0", true),
        ("0.9.10", true),
        ("0.9.3-dev.1", true),
        ("0.9.2", false),
        ("0.8.99", false),
        ("garbage", false),
    ] {
        let fake = FakeHerdr::start().await;
        fake.reply("ping", pong(version, 22));
        let result = client(&fake).check_compat().await;
        assert_eq!(result.is_ok(), ok, "{version}: {result:?}");
    }
}

#[tokio::test]
async fn unknown_type_tag_is_incompatible_for_that_call_only() {
    let fake = FakeHerdr::start().await;
    fake.reply("agent.get", json!({"type": "brand_new_thing", "x": 1}));
    fake.reply(
        "agent.get",
        json!({"type": "agent_info", "agent": agent_json()}),
    );
    let client = client(&fake);

    let err = client.agent_get("w1:p1").await.unwrap_err();
    assert!(matches!(err, HerdrError::Incompatible { .. }), "{err:?}");

    let agent = client.agent_get("w1:p1").await.unwrap();
    assert_eq!(agent.pane_id, "w1:p1");
}

#[tokio::test]
async fn wrong_result_type_for_method_is_incompatible() {
    let fake = FakeHerdr::start().await;
    fake.reply("pane.read", json!({"type": "ok"}));
    let err = client(&fake)
        .pane_read(&PaneReadParams::new("w1:p1"))
        .await
        .unwrap_err();
    assert!(matches!(err, HerdrError::Incompatible { .. }), "{err:?}");
}

#[tokio::test]
async fn method_rejected_as_unknown_is_incompatible() {
    for (code, message) in [
        ("unknown_method", "unknown method: agent.explain"),
        (
            "invalid_request",
            "invalid request: unknown variant `agent.explain`, expected one of `ping`",
        ),
    ] {
        let fake = FakeHerdr::start().await;
        fake.on("agent.explain", Script::error(code, message));
        let err = client(&fake).agent_explain("w1:p1").await.unwrap_err();
        assert!(
            matches!(err, HerdrError::Incompatible { .. }),
            "{code}: {err:?}"
        );
    }
}

#[tokio::test]
async fn other_invalid_request_stays_a_typed_api_error() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "agent.explain",
        Script::error("invalid_request", "invalid request: missing field `target`"),
    );
    let err = client(&fake).agent_explain("x").await.unwrap_err();
    assert!(
        matches!(
            &err,
            HerdrError::Api {
                code: ApiErrorCode::InvalidRequest,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn agent_get_round_trip() {
    let fake = FakeHerdr::start().await;
    fake.reply(
        "agent.get",
        json!({"type": "agent_info", "agent": agent_json()}),
    );
    let agent = client(&fake).agent_get("w1:p1").await.unwrap();

    let request = fake.last_request();
    assert_eq!(request["method"], "agent.get");
    assert_eq!(request["params"], json!({"target": "w1:p1"}));
    assert!(request["id"].is_string());

    assert_eq!(agent.agent_status, AgentStatus::Working);
    assert_eq!(agent.agent.as_deref(), Some("claude"));
    assert_eq!(agent.tokens["role"], "reviewer");
    assert_eq!(agent.state_change_seq, 7);
    assert_eq!(agent.completion_seq, Some(3));
    assert!(agent.interactive_ready);
    assert!(!agent.launch_pending);
    assert_eq!(agent.revision, 42);
}

#[tokio::test]
async fn pane_read_round_trip() {
    let fake = FakeHerdr::start().await;
    fake.reply(
        "pane.read",
        json!({"type": "pane_read", "read": read_json()}),
    );
    let params = PaneReadParams {
        source: ReadSource::RecentUnwrapped,
        lines: Some(50),
        ..PaneReadParams::new("w1:p1")
    };
    let read = client(&fake).pane_read(&params).await.unwrap();

    assert_eq!(
        fake.last_request()["params"],
        json!({"pane_id": "w1:p1", "source": "recent_unwrapped", "lines": 50,
               "format": "text", "strip_ansi": true})
    );
    assert_eq!(read.text, "hello\nworld");
    assert_eq!(read.revision, 9);
    assert!(!read.truncated);
}

#[tokio::test]
async fn session_snapshot_round_trip() {
    let fake = FakeHerdr::start().await;
    fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": {
            "version": "0.9.5", "protocol": 24, "focused_pane_id": "w1:p1",
            "workspaces": [workspace_json()], "tabs": [tab_json()],
            "panes": [pane_json()], "layouts": [{"workspace_id": "w1"}],
            "agents": [agent_json()]
        }}),
    );
    let snapshot = client(&fake).session_snapshot().await.unwrap();

    assert_eq!(fake.last_request()["method"], "session.snapshot");
    assert_eq!(fake.last_request()["params"], json!({}));
    assert_eq!(snapshot.protocol, 24);
    assert_eq!(snapshot.focused_pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(snapshot.workspaces[0].tab_count, 1);
    assert_eq!(snapshot.tabs[0].label, "work");
    assert_eq!(snapshot.panes[0].pane_id, "w1:p2");
    assert_eq!(snapshot.agents[0].name.as_deref(), Some("worker"));
    assert_eq!(snapshot.layouts.len(), 1);
}

#[tokio::test]
async fn error_response_maps_to_typed_error() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "agent.get",
        Script::error("agent_not_running", "no agent in w1:p1"),
    );
    let err = client(&fake).agent_get("w1:p1").await.unwrap_err();
    match &err {
        HerdrError::Api { code, message } => {
            assert_eq!(*code, ApiErrorCode::AgentNotRunning);
            assert_eq!(message, "no agent in w1:p1");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(err.code(), "agent_not_running");
}

#[tokio::test]
async fn unrecognised_error_code_is_preserved() {
    let fake = FakeHerdr::start().await;
    fake.on("agent.get", Script::error("something_new", "m"));
    let err = client(&fake).agent_get("x").await.unwrap_err();
    assert!(
        matches!(&err, HerdrError::Api { code: ApiErrorCode::Other(c), .. } if c == "something_new"),
        "{err:?}"
    );
    assert_eq!(err.code(), "something_new");
}

#[tokio::test]
async fn herdr_timeout_is_an_outcome_not_an_error() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "agent.prompt",
        Script::error("timeout", "timed out waiting for agent status"),
    );
    let params = AgentPromptParams::new("w1:p1", "go");
    let outcome = client(&fake).agent_prompt(&params).await.unwrap();
    assert!(
        matches!(&outcome, Outcome::TimedOut { message } if message.contains("timed out")),
        "{outcome:?}"
    );

    fake.on("agent.start", Script::error("timeout", "startup timed out"));
    let params = AgentStartParams::new("n", "claude", "w1:p1");
    let outcome = client(&fake).agent_start(&params).await.unwrap();
    assert!(matches!(outcome, Outcome::TimedOut { .. }));
}

#[tokio::test]
async fn response_line_over_1_mib_is_a_bounded_error() {
    let fake = FakeHerdr::start().await;
    fake.on("pane.read", Script::Raw("a".repeat(1024 * 1024 + 16)));
    let err = client(&fake)
        .pane_read(&PaneReadParams::new("w1:p1"))
        .await
        .unwrap_err();
    assert!(matches!(err, HerdrError::LineTooLong { .. }), "{err:?}");
}

#[tokio::test]
async fn connection_closed_without_response_is_a_protocol_error() {
    let fake = FakeHerdr::start().await;
    fake.on("ping", Script::Drop);
    let err = client(&fake).ping().await.unwrap_err();
    assert!(matches!(err, HerdrError::Protocol { .. }), "{err:?}");
}

#[tokio::test]
async fn each_call_uses_its_own_connection_and_drops_it() {
    let fake = FakeHerdr::start().await;
    fake.reply("ping", pong("0.9.5", 24));
    fake.reply("ping", pong("0.9.5", 24));
    let client = client(&fake);
    client.ping().await.unwrap();
    client.ping().await.unwrap();
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn subscription_receives_three_events_in_order_then_reports_events_lost() {
    let fake = FakeHerdr::start().await;
    let events = (1..=3)
        .map(|n| json!({"event": "pane_created", "data": {"seq": n}}))
        .collect();
    fake.on(
        "events.subscribe",
        Script::Stream {
            events,
            end: StreamEnd::Lost,
        },
    );

    let mut sub = client(&fake)
        .subscribe(vec![json!({"type": "pane.created"})])
        .await
        .unwrap();

    let request = fake.last_request();
    assert_eq!(request["method"], "events.subscribe");
    assert_eq!(
        request["params"],
        json!({"subscriptions": [{"type": "pane.created"}]})
    );

    for n in 1..=3 {
        match sub.next().await {
            Some(SubscriptionEvent::Event(event)) => {
                assert_eq!(event.event, "pane_created");
                assert_eq!(event.data["seq"], n);
            }
            other => panic!("event {n}: {other:?}"),
        }
    }
    assert!(matches!(
        sub.next().await,
        Some(SubscriptionEvent::EventsLost)
    ));
    assert!(sub.next().await.is_none());
}

#[tokio::test]
async fn subscription_reports_closed_when_server_hangs_up() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![json!({"event": "tab_created", "data": {}})],
            end: StreamEnd::Close,
        },
    );
    let mut sub = client(&fake).subscribe(vec![]).await.unwrap();
    assert!(matches!(
        sub.next().await,
        Some(SubscriptionEvent::Event(_))
    ));
    assert!(matches!(sub.next().await, Some(SubscriptionEvent::Closed)));
    assert!(sub.next().await.is_none());
}

#[tokio::test]
async fn subscribe_rejection_is_a_typed_error() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "events.subscribe",
        Script::error("pane_not_found", "pane w9:p9 not found"),
    );
    let err = client(&fake).subscribe(vec![]).await.unwrap_err();
    assert!(
        matches!(
            err,
            HerdrError::Api {
                code: ApiErrorCode::PaneNotFound,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn dropping_a_subscription_closes_its_connection() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Hold,
        },
    );
    let sub = client(&fake).subscribe(vec![]).await.unwrap();
    drop(sub);
    fake.wait_disconnect().await;
}

#[tokio::test]
async fn dropping_a_pending_wait_closes_the_connection() {
    let fake = FakeHerdr::start().await;
    fake.on("agent.prompt", Script::Hold);
    let client = client(&fake);
    let mut params = AgentPromptParams::new("w1:p1", "go");
    params.wait = Some(Default::default());

    let pending =
        tokio::time::timeout(Duration::from_millis(100), client.agent_prompt(&params)).await;
    assert!(pending.is_err(), "call should still be pending");

    fake.wait_disconnect().await;
}

#[tokio::test]
async fn read_and_control_methods_send_the_expected_wire_requests() {
    let fake = FakeHerdr::start().await;
    let c = client(&fake);

    fake.reply(
        "agent.list",
        json!({"type": "agent_list", "agents": [agent_json()]}),
    );
    assert_eq!(c.agent_list().await.unwrap().len(), 1);
    assert_eq!(fake.last_request()["params"], json!({}));

    fake.reply(
        "agent.read",
        json!({"type": "pane_read", "read": read_json()}),
    );
    c.agent_read(&herdr_mcp::herdr::AgentReadParams::new("w1:p1"))
        .await
        .unwrap();
    assert_eq!(fake.last_request()["method"], "agent.read");
    assert_eq!(fake.last_request()["params"]["target"], "w1:p1");

    fake.reply(
        "agent.explain",
        json!({"type": "agent_explain", "explain": {"rule": "x"}}),
    );
    assert_eq!(c.agent_explain("w1:p1").await.unwrap()["rule"], "x");

    fake.reply("agent.send_keys", json!({"type": "ok"}));
    c.agent_send_keys("w1:p1", &["Escape".into()])
        .await
        .unwrap();
    assert_eq!(
        fake.last_request()["params"],
        json!({"target": "w1:p1", "keys": ["Escape"]})
    );

    fake.reply(
        "agent.start",
        json!({"type": "agent_started", "agent": agent_json(), "argv": ["claude", "--x"]}),
    );
    let started = c
        .agent_start(&AgentStartParams {
            args: vec!["--x".into()],
            timeout_ms: Some(10_000),
            ..AgentStartParams::new("n", "claude", "w1:p1")
        })
        .await
        .unwrap();
    assert!(matches!(&started, Outcome::Done(s) if s.argv == ["claude", "--x"]));
    assert_eq!(
        fake.last_request()["params"],
        json!({"name": "n", "kind": "claude", "pane_id": "w1:p1",
               "args": ["--x"], "timeout_ms": 10000})
    );

    fake.reply(
        "agent.prompt",
        json!({"type": "agent_prompted", "agent": agent_json()}),
    );
    let mut prompt = AgentPromptParams::new("w1:p1", "go");
    prompt.wait = Some(herdr_mcp::herdr::PromptWait {
        until: vec![AgentStatus::Idle, AgentStatus::Done],
        timeout_ms: Some(1000),
    });
    assert!(matches!(
        c.agent_prompt(&prompt).await.unwrap(),
        Outcome::Done(_)
    ));
    assert_eq!(
        fake.last_request()["params"],
        json!({"target": "w1:p1", "text": "go",
               "wait": {"until": ["idle", "done"], "timeout_ms": 1000}})
    );

    fake.reply(
        "pane.process_info",
        json!({"type": "pane_process_info", "process_info": {
            "pane_id": "w1:p1", "shell_pid": 10, "foreground_process_group_id": 11,
            "tty": "/dev/ttys001",
            "foreground_processes": [{"pid": 11, "name": "cargo", "cmdline": "cargo test"}]}}),
    );
    let info = c.pane_process_info(Some("w1:p1")).await.unwrap();
    assert_eq!(info.foreground_processes[0].name, "cargo");
    assert_eq!(fake.last_request()["params"], json!({"pane_id": "w1:p1"}));

    fake.reply("pane.send_input", json!({"type": "ok"}));
    c.pane_send_input(&PaneSendInputParams {
        pane_id: "w1:p1".into(),
        text: "ls".into(),
        keys: vec!["Enter".into()],
    })
    .await
    .unwrap();
    assert_eq!(
        fake.last_request()["params"],
        json!({"pane_id": "w1:p1", "text": "ls", "keys": ["Enter"]})
    );

    fake.reply("pane.send_text", json!({"type": "ok"}));
    c.pane_send_text("w1:p1", "hi").await.unwrap();
    assert_eq!(
        fake.last_request()["params"],
        json!({"pane_id": "w1:p1", "text": "hi"})
    );

    fake.reply("pane.send_keys", json!({"type": "ok"}));
    c.pane_send_keys("w1:p1", &["C-c".into()]).await.unwrap();
    assert_eq!(
        fake.last_request()["params"],
        json!({"pane_id": "w1:p1", "keys": ["C-c"]})
    );

    fake.reply(
        "pane.split",
        json!({"type": "pane_info", "pane": pane_json()}),
    );
    let pane = c
        .pane_split(&PaneSplitParams {
            target_pane_id: Some("w1:p1".into()),
            direction: SplitDirection::Down,
            ..PaneSplitParams::default()
        })
        .await
        .unwrap();
    assert_eq!(pane.pane_id, "w1:p2");
    assert_eq!(
        fake.last_request()["params"],
        json!({"target_pane_id": "w1:p1", "direction": "down", "focus": false})
    );

    fake.reply("pane.close", json!({"type": "ok"}));
    c.pane_close("w1:p2").await.unwrap();
    assert_eq!(fake.last_request()["params"], json!({"pane_id": "w1:p2"}));

    fake.reply(
        "tab.create",
        json!({"type": "tab_created", "tab": tab_json(), "root_pane": pane_json()}),
    );
    let created = c
        .tab_create(&TabCreateParams {
            workspace_id: Some("w1".into()),
            label: Some("work".into()),
            ..TabCreateParams::default()
        })
        .await
        .unwrap();
    assert_eq!(created.tab.tab_id, "w1:t2");
    assert_eq!(
        fake.last_request()["params"],
        json!({"workspace_id": "w1", "label": "work", "focus": false})
    );

    fake.reply("tab.close", json!({"type": "ok"}));
    c.tab_close("w1:t2").await.unwrap();
    assert_eq!(fake.last_request()["params"], json!({"tab_id": "w1:t2"}));

    fake.reply(
        "workspace.create",
        json!({"type": "workspace_created", "workspace": workspace_json(),
               "tab": tab_json(), "root_pane": pane_json()}),
    );
    let ws = c
        .workspace_create(&WorkspaceCreateParams {
            cwd: Some("/tmp".into()),
            ..WorkspaceCreateParams::default()
        })
        .await
        .unwrap();
    assert_eq!(ws.workspace.workspace_id, "w2");

    let worktree = json!({"path": "/wt", "branch": "b", "is_bare": false, "is_detached": false,
        "is_prunable": false, "is_linked_worktree": true, "label": "b"});
    fake.reply(
        "worktree.create",
        json!({"type": "worktree_created", "workspace": workspace_json(), "tab": tab_json(),
               "root_pane": pane_json(), "worktree": worktree}),
    );
    let wt = c
        .worktree_create(&WorktreeCreateParams {
            branch: Some("b".into()),
            ..WorktreeCreateParams::default()
        })
        .await
        .unwrap();
    assert_eq!(wt.worktree.branch.as_deref(), Some("b"));
    assert!(wt.worktree.is_linked_worktree);

    fake.reply(
        "worktree.list",
        json!({"type": "worktree_list",
               "source": {"repo_key": "k", "repo_name": "r", "repo_root": "/r",
                          "source_checkout_path": "/r"},
               "worktrees": [worktree]}),
    );
    let list = c
        .worktree_list(&WorktreeListParams::default())
        .await
        .unwrap();
    assert_eq!(list.source.repo_name, "r");
    assert_eq!(list.worktrees.len(), 1);
}
