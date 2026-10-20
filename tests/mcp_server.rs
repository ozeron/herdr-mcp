#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fake_herdr::{FakeHerdr, Script};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::{ClientKind, Profile};
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const SENTINEL: &str = "SENTINEL-d41d8cd9";

struct Opts {
    mode: ToolMode,
    client_flag: Option<ClientKind>,
    max_wait: Option<Duration>,
    client_name: &'static str,
    ancestors: Vec<u32>,
    env_pane: Option<&'static str>,
    socket: Option<PathBuf>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            mode: ToolMode::Read,
            client_flag: None,
            max_wait: None,
            client_name: "test-client",
            ancestors: vec![9000, 4242, 1],
            env_pane: Some("w1:p1"),
            socket: None,
        }
    }
}

struct Harness {
    fake: FakeHerdr,
    client: RunningService<RoleClient, ClientConfig>,
    server: JoinHandle<()>,
    log_dir: TempDir,
}

async fn start(opts: Opts) -> Harness {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: opts.socket.unwrap_or_else(|| fake.path().to_path_buf()),
        profile: Profile::new(opts.client_flag, opts.max_wait),
        mode: opts.mode,
        ancestry: Arc::new(StaticAncestry(opts.ancestors)),
        caller_env: CallerEnv {
            pane_id: opts.env_pane.map(Into::into),
            tab_id: opts.env_pane.map(|_| "w1:t1".into()),
            workspace_id: opts.env_pane.map(|_| "w1".into()),
            herdr_env_present: opts.env_pane.is_some(),
        },
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
    };
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let running = HerdrMcp::new(config).serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new(opts.client_name, "1.0.0"),
    )
    .serve(client_io)
    .await
    .unwrap();
    Harness {
        fake,
        client,
        server,
        log_dir,
    }
}

fn script_capabilities(fake: &FakeHerdr) {
    fake.reply(
        "ping",
        json!({"type": "pong", "version": "0.9.3", "protocol": 22}),
    );
    fake.reply(
        "pane.process_info",
        json!({"type": "pane_process_info", "process_info": {
            "pane_id": "w1:p1", "shell_pid": 4242, "foreground_process_group_id": 4242,
            "tty": "/dev/ttys001",
            "foreground_processes": [{"pid": 4242, "name": "zsh", "cmdline": SENTINEL}]}}),
    );
    fake.reply(
        "server.agent_manifests",
        json!({"type": "agent_manifest_status", "last_check_unix": null, "last_result": null,
        "manifests": [
            {"agent": "claude", "source": "embedded", "source_kind": "embedded",
             "active_version": "2.1.0", "warning": SENTINEL},
            {"agent": "codex", "source": "embedded", "source_kind": "embedded",
             "active_version": "1.4.2"},
            {"agent": "pi", "source": "embedded", "source_kind": "embedded",
             "active_version": null}
        ]}),
    );
}

async fn call(h: &Harness, name: &'static str) -> CallToolResult {
    h.client
        .call_tool(CallToolRequestParams::new(name))
        .await
        .unwrap()
}

async fn capabilities(h: &Harness) -> Value {
    script_capabilities(&h.fake);
    let result = call(h, "herdr_capabilities").await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.expect("structured content")
}

async fn wait_cap_ms(opts: Opts) -> u64 {
    let h = start(opts).await;
    capabilities(&h).await["wait_cap_ms"].as_u64().unwrap()
}

#[tokio::test]
async fn read_mode_lists_exactly_the_four_read_tools() {
    let h = start(Opts::default()).await;
    let names: BTreeSet<String> = h
        .client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let expected: BTreeSet<String> = [
        "herdr_capabilities",
        "herdr_list",
        "herdr_read",
        "herdr_wait",
    ]
    .map(String::from)
    .into();
    assert_eq!(names, expected);
}

#[tokio::test]
async fn read_mode_rejects_control_tool_calls() {
    let h = start(Opts::default()).await;
    let err = h
        .client
        .call_tool(CallToolRequestParams::new("herdr_send"))
        .await;
    assert!(err.is_err(), "{err:?}");
}

#[tokio::test]
async fn control_mode_lists_all_nine_with_annotations() {
    let h = start(Opts {
        mode: ToolMode::Control,
        ..Opts::default()
    })
    .await;
    let tools = h.client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 9);
    let read_only = [
        "herdr_capabilities",
        "herdr_list",
        "herdr_read",
        "herdr_wait",
    ];
    let destructive = [
        "herdr_launch",
        "herdr_run",
        "herdr_send",
        "herdr_interrupt",
        "herdr_close",
    ];
    for tool in &tools {
        let name = tool.name.as_ref();
        let ann = tool.annotations.as_ref().expect(name);
        if read_only.contains(&name) {
            assert_eq!(ann.read_only_hint, Some(true), "{name}");
        } else {
            assert!(destructive.contains(&name), "unexpected tool {name}");
            assert_eq!(ann.destructive_hint, Some(true), "{name}");
            assert_ne!(ann.read_only_hint, Some(true), "{name}");
        }
        assert!(
            tool.description.as_deref().is_some_and(|d| !d.is_empty()),
            "{name}"
        );
    }
}

#[tokio::test]
async fn control_mode_stubs_return_not_implemented() {
    let h = start(Opts {
        mode: ToolMode::Control,
        ..Opts::default()
    })
    .await;
    for name in [
        "herdr_list",
        "herdr_read",
        "herdr_wait",
        "herdr_launch",
        "herdr_run",
        "herdr_send",
        "herdr_interrupt",
        "herdr_close",
    ] {
        let result = call(&h, name).await;
        assert_eq!(result.is_error, Some(true), "{name}");
        assert_eq!(
            result.structured_content.unwrap()["code"],
            "not_implemented",
            "{name}"
        );
    }
    assert!(h.fake.requests().is_empty());
}

#[tokio::test]
async fn instructions_are_short_and_lead_with_the_essentials() {
    let h = start(Opts::default()).await;
    let info = h.client.peer_info().unwrap();
    let text = info.instructions.clone().expect("instructions");
    assert!(text.chars().count() < 4096, "{}", text.chars().count());
    let head: String = text.chars().take(512).collect::<String>().to_lowercase();
    assert!(head.contains("idle"), "{head}");
    assert!(head.contains("bounded"), "{head}");
    assert!(head.contains("untrusted"), "{head}");
    let all = text.to_lowercase();
    for needle in [
        "timed_out",
        "steer",
        "own pane",
        "launched",
        "blocked",
        "owner",
    ] {
        assert!(all.contains(needle), "missing {needle}");
    }
}

#[tokio::test]
async fn capabilities_reports_caller_versions_and_agents() {
    let h = start(Opts {
        client_flag: Some(ClientKind::Claude),
        ..Opts::default()
    })
    .await;
    let caps = capabilities(&h).await;

    assert_eq!(caps["caller"]["pane_id"], "w1:p1");
    assert_eq!(caps["caller_verified"], true);
    assert_eq!(caps["guard_target"], "w1:p1");
    assert_eq!(caps["tool_mode"], "read");
    assert_eq!(caps["herdr"]["version"], "0.9.3");
    assert_eq!(caps["herdr"]["protocol"], 22);
    assert_eq!(caps["wait_cap_ms"], 600_000);
    let agents = caps["agents"].as_array().unwrap();
    let find = |kind: &str| agents.iter().find(|a| a["kind"] == kind).unwrap();
    assert_eq!(find("claude")["mcp"], "injectable");
    assert_eq!(find("claude")["active_version"], "2.1.0");
    assert_eq!(find("codex")["mcp"], "injectable");
    assert_eq!(find("codex")["active_version"], "1.4.2");
    assert_eq!(find("pi")["mcp"], "unsupported");
    assert!(find("pi")["active_version"].is_null());
}

#[tokio::test]
async fn unverified_caller_is_reported_with_its_guard_target() {
    let h = start(Opts {
        ancestors: vec![9000, 1],
        ..Opts::default()
    })
    .await;
    h.fake.reply(
        "ping",
        json!({"type": "pong", "version": "0.9.3", "protocol": 22}),
    );
    h.fake.reply(
        "pane.process_info",
        json!({"type": "pane_process_info", "process_info": {
            "pane_id": "w1:p1", "shell_pid": 4242, "foreground_processes": []}}),
    );
    h.fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": {
            "version": "0.9.3", "protocol": 22, "workspaces": [], "tabs": [],
            "panes": [], "layouts": [], "agents": []}}),
    );
    h.fake.reply(
        "server.agent_manifests",
        json!({"type": "agent_manifest_status", "manifests": []}),
    );
    let caps = call(&h, "herdr_capabilities")
        .await
        .structured_content
        .unwrap();
    assert_eq!(caps["caller_verified"], false);
    assert_eq!(caps["guard_target"], "w1:p1");
}

#[tokio::test]
async fn herdr_not_running_is_a_tool_error_and_the_server_stays_up() {
    let gone = {
        let fake = FakeHerdr::start().await;
        fake.path().to_path_buf()
    };
    let h = start(Opts {
        socket: Some(gone),
        ..Opts::default()
    })
    .await;

    let result = call(&h, "herdr_capabilities").await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.unwrap()["code"],
        "herdr_not_running"
    );
    assert_eq!(h.client.list_all_tools().await.unwrap().len(), 4);
}

#[tokio::test]
async fn wait_cap_follows_the_client_profile() {
    let codex_flag = wait_cap_ms(Opts {
        client_flag: Some(ClientKind::Codex),
        ..Opts::default()
    })
    .await;
    assert_eq!(codex_flag, 280_000);

    let codex_info = wait_cap_ms(Opts {
        client_name: "codex-mcp-client",
        ..Opts::default()
    })
    .await;
    assert_eq!(codex_info, 280_000);

    let unknown = wait_cap_ms(Opts::default()).await;
    assert_eq!(unknown, 50_000);

    let claude_flag = wait_cap_ms(Opts {
        client_flag: Some(ClientKind::Claude),
        client_name: "codex-mcp-client",
        ..Opts::default()
    })
    .await;
    assert_eq!(claude_flag, 600_000);

    let overridden = wait_cap_ms(Opts {
        client_flag: Some(ClientKind::Codex),
        client_name: "codex-mcp-client",
        max_wait: Some(Duration::from_millis(1234)),
        ..Opts::default()
    })
    .await;
    assert_eq!(overridden, 1234);
}

#[tokio::test]
async fn a_pending_call_does_not_block_a_second_call() {
    let h = start(Opts::default()).await;
    h.fake.on("ping", Script::Hold);
    let peer = h.client.peer().clone();
    let first = tokio::spawn(async move {
        peer.call_tool(CallToolRequestParams::new("herdr_capabilities"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.fake.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first call never reached herdr");

    script_capabilities(&h.fake);
    let started = std::time::Instant::now();
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        h.client
            .call_tool(CallToolRequestParams::new("herdr_capabilities")),
    )
    .await
    .expect("second call blocked behind the first")
    .unwrap();
    assert_ne!(second.is_error, Some(true), "{second:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!first.is_finished());
    first.abort();
}

#[tokio::test]
async fn logs_never_contain_arguments_or_herdr_output() {
    use std::os::unix::fs::PermissionsExt;

    let h = start(Opts::default()).await;
    script_capabilities(&h.fake);
    let mut params = CallToolRequestParams::new("herdr_capabilities");
    params.arguments = json!({"text": SENTINEL, "keys": SENTINEL, "command": SENTINEL})
        .as_object()
        .cloned();
    let result = h.client.call_tool(params).await.unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");

    let dir = h.log_dir.path().join("logs");
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut total = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        assert_eq!(
            entry.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(entry.path()).unwrap();
        assert!(!text.contains(SENTINEL), "{text}");
        total += text.len();
    }
    assert!(total > 0, "nothing was logged");
}

#[tokio::test]
async fn bridge_exits_when_stdin_closes() {
    let h = start(Opts::default()).await;
    let Harness {
        mut client, server, ..
    } = h;
    client.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server did not exit")
        .unwrap();
}
