#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_herdr::{FakeHerdr, Script, StreamEnd};
use harness::{
    AGENT_PARENT, Harness, PARENT, call, count, err, err_code, ok, ok_reply, pane, process_info,
    requests, tab, workspace,
};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::herdr::HerdrError;
use herdr_mcp::install::InstallEnv;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp, Inner};
use herdr_mcp::tools::launch::{self, LaunchTuning};
use herdr_mcp::tools::send::{self, SendParams};
use herdr_mcp::wait::Tuning;
use rmcp::ServiceExt;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const QUIET: Duration = Duration::from_millis(120);

fn launch_tuning() -> LaunchTuning {
    LaunchTuning {
        start_retry_interval: Duration::from_millis(5),
        start_retry_window: Duration::from_millis(300),
        ready_poll: Duration::from_millis(5),
        quiet_window: Duration::from_millis(40),
        dialog_settle: Duration::from_millis(60),
        idle_poll: Duration::from_millis(5),
        startup_grace: Duration::from_millis(200),
        mail_poll: Duration::from_millis(10),
        reply_grace: Duration::from_millis(300),
    }
}

fn wait_tuning() -> Tuning {
    Tuning {
        poll_interval: Duration::from_millis(20),
        progress_interval: Duration::from_secs(60),
        observe_window: Duration::from_millis(150),
        recheck_delay: Duration::from_millis(30),
        pane_interval: Duration::from_millis(10),
    }
}

struct Sandbox {
    _tmp: TempDir,
    env: InstallEnv,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let exe = root.join("herdr-mcp-exe");
        fs::write(&exe, "").unwrap();
        let env = InstallEnv {
            home: root.join("home"),
            claude_config_dir: None,
            codex_home: None,
            path: OsString::new(),
            exe,
        };
        Self { _tmp: tmp, env }
    }

    fn shim(&self) -> PathBuf {
        self.env.shim_path()
    }

    fn link_shim(&self) {
        fs::create_dir_all(self.shim().parent().unwrap()).unwrap();
        symlink(&self.env.exe, self.shim()).unwrap();
    }

    fn claude_entry_is_ours(&self) {
        self.claude_entry(ToolMode::Read);
    }

    fn claude_entry(&self, mode: ToolMode) {
        self.link_shim();
        let config = json!({"mcpServers": {"herdr": {
            "type": "stdio", "command": self.shim(),
            "args": ["--client", "claude", "--tools", mode.as_str()], "env": {}}}});
        fs::write(self.env.home.join(".claude.json"), config.to_string()).unwrap();
    }

    fn codex_entry(&self, mode: ToolMode) {
        self.link_shim();
        let dir = self.env.home.join(".codex");
        fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            "[mcp_servers.herdr]\ncommand = {}\nargs = [\"--client\", \"codex\", \"--tools\", \"{}\"]\n",
            json!(self.shim().display().to_string()),
            mode.as_str()
        );
        fs::write(dir.join("config.toml"), toml).unwrap();
    }

    fn bin(&self) -> String {
        if self.shim().exists() {
            self.shim().display().to_string()
        } else {
            self.env.exe.display().to_string()
        }
    }
}

struct Opts {
    env_pane: Option<&'static str>,
    install_env: Option<InstallEnv>,
    max_wait: Option<Duration>,
    quiet: Duration,
    mailbox: Option<PathBuf>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env_pane: Some("w1:p1"),
            install_env: None,
            max_wait: Some(Duration::from_secs(5)),
            quiet: Duration::from_millis(40),
            mailbox: None,
        }
    }
}

fn config(fake: &FakeHerdr, opts: &Opts, log_dir: &TempDir, mode: ToolMode) -> BridgeConfig {
    BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, opts.max_wait),
        mode,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv {
            pane_id: opts.env_pane.map(Into::into),
            tab_id: opts.env_pane.map(|_| "w1:t1".into()),
            workspace_id: opts.env_pane.map(|_| "w1".into()),
        },
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
        install_env: opts.install_env.clone(),
    }
}

async fn start(opts: Opts) -> Harness {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let config = config(&fake, &opts, &log_dir, ToolMode::Control);
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    let tuning = LaunchTuning {
        quiet_window: opts.quiet,
        dialog_settle: Duration::from_millis(60),
        ..launch_tuning()
    };
    let mailbox = opts
        .mailbox
        .clone()
        .unwrap_or_else(|| log_dir.path().join("state"));
    tokio::spawn(async move {
        let mut server = HerdrMcp::new(config)
            .with_wait_tuning(wait_tuning())
            .with_interrupt_settle(Duration::from_millis(1))
            .with_launch_tuning(tuning);
        server = server.with_mailbox_state_dir(mailbox);
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

fn direct_inner(fake: &FakeHerdr, mode: ToolMode, log_dir: &TempDir) -> Inner {
    let config = config(fake, &Opts::default(), log_dir, mode);
    Inner {
        sessions: herdr_mcp::sessions::Sessions::new(
            config.socket_path,
            herdr_mcp::herdr::SessionEnv::default(),
        ),
        profile: config.profile,
        mode: config.mode,
        ancestry: config.ancestry,
        caller_env: config.caller_env,
        logger: config.logger,
        install_env: None,
        wait: wait_tuning(),
        interrupt_settle: Duration::from_millis(1),
        launch: launch_tuning(),
        mailbox_state: log_dir.path().join("state"),
        tunnel: None,
    }
}

struct Rev {
    status: &'static str,
    seq: u64,
    parent: Option<&'static str>,
    kind: &'static str,
}

impl Rev {
    fn json(&self) -> Value {
        let mut agent = json!({
            "terminal_id": "t-w1:p4", "name": "rev", "agent": self.kind,
            "agent_status": self.status, "pane_id": "w1:p4", "tab_id": "w1:t2",
            "workspace_id": "w1", "focused": false, "revision": 1,
            "state_change_seq": self.seq, "interactive_ready": true, "launch_pending": false,
        });
        if let Some(parent) = self.parent {
            agent["tokens"] = json!({ PARENT: parent });
        }
        agent
    }
}

fn rev(status: &'static str, seq: u64) -> Value {
    json!({"type": "agent_info", "agent": Rev {status, seq, parent: Some("w1:p1"), kind: "codex"}.json()})
}

fn rev_with(status: &'static str, seq: u64, parent: Option<&'static str>) -> Value {
    json!({"type": "agent_info", "agent": Rev {status, seq, parent, kind: "codex"}.json()})
}

fn prompted(status: &'static str, seq: u64) -> Value {
    json!({"type": "agent_prompted", "agent": Rev {status, seq, parent: Some("w1:p1"), kind: "codex"}.json()})
}

fn kid_json(status: &str, seq: u64, completion: Option<u64>, ready: bool, pending: bool) -> Value {
    json!({
        "terminal_id": "t-w1:p9", "name": "kid", "agent": "codex", "agent_status": status,
        "pane_id": "w1:p9", "tab_id": "w1:t9", "workspace_id": "w1", "focused": false,
        "revision": 1, "state_change_seq": seq, "completion_seq": completion,
        "interactive_ready": ready, "launch_pending": pending, "tokens": { PARENT: "w1:p1" },
    })
}

fn kid(status: &str, seq: u64, ready: bool, pending: bool) -> Value {
    json!({"type": "agent_info", "agent": kid_json(status, seq, None, ready, pending)})
}

/// w1:t1 holds the caller w1:p1 and the human pane w1:p2.
/// w1:t2 holds w1:p3 (a plain pane) and w1:p4 (the agent "rev"); the caller launched both.
/// w1:t9 holds w1:p9, the pane a launch creates, when `with_new` is set.
fn snapshot(with_new: bool, rev_tokens: Option<&'static str>) -> Value {
    let mut panes = vec![
        pane("w1:p1", "w1:t1", "w1", None),
        pane("w1:p2", "w1:t1", "w1", None),
        pane("w1:p3", "w1:t2", "w1", Some("w1:p1")),
        pane("w1:p4", "w1:t2", "w1", rev_tokens),
    ];
    let mut tabs = vec![tab("w1:t1", "w1", 1), tab("w1:t2", "w1", 2)];
    if with_new {
        panes.push(pane("w1:p9", "w1:t9", "w1", Some("w1:p1")));
        tabs.push(tab("w1:t9", "w1", 3));
    }
    json!({"type": "session_snapshot", "snapshot": {
        "version": "0.9.3", "protocol": 22, "layouts": [],
        "focused_workspace_id": "w1", "focused_tab_id": "w1:t1", "focused_pane_id": "w1:p1",
        "workspaces": [workspace("w1", 1, "w1:t1")], "tabs": tabs, "panes": panes,
        "agents": [Rev {status: "idle", seq: 3, parent: rev_tokens, kind: "codex"}.json()],
    }})
}

fn read_result(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": "w1:p9", "workspace_id": "w1", "tab_id": "w1:t9", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 7, "truncated": false}})
}

fn tab_created() -> Value {
    json!({"type": "tab_created", "tab": tab("w1:t9", "w1", 3),
           "root_pane": pane("w1:p9", "w1:t9", "w1", None)})
}

fn started() -> Value {
    json!({"type": "agent_started", "agent": kid_json("unknown", 1, None, false, true),
           "argv": ["codex"]})
}

fn caller_ok(fake: &FakeHerdr) {
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
}

struct Launch {
    busy: usize,
    ready: Vec<Value>,
}

fn script_launch(fake: &FakeHerdr, spec: Launch) {
    let done = json!({"type": "agent_info", "agent": kid_json("done", 9, Some(9), true, false)});
    script_launch_then(fake, spec, done);
}

/// `after_prompt` is what `agent.get` reports once the prompt has been sent.
fn script_launch_then(fake: &FakeHerdr, spec: Launch, after_prompt: Value) {
    script_launch_with(fake, spec, Script::Reply(after_prompt));
}

fn script_launch_with(fake: &FakeHerdr, spec: Launch, after_prompt: Script) {
    caller_ok(fake);
    fake.reply_seq(
        "session.snapshot",
        vec![snapshot(false, None), snapshot(true, None)],
    );
    fake.reply("tab.create", tab_created());
    fake.reply_seq("pane.report_metadata", vec![ok_reply()]);
    let mut starts: Vec<Script> = (0..spec.busy)
        .map(|_| Script::error("agent_pane_busy", "pane is busy"))
        .collect();
    starts.push(Script::Reply(started()));
    fake.on_seq("agent.start", starts);
    let mut fronts = Vec::new();
    if spec.busy > 0 {
        fronts.push(process_info("w1:p9", 200, 200));
    }
    fronts.push(process_info("w1:p9", 200, 300));
    fake.reply_seq("pane.process_info", fronts);
    fake.reply_seq("agent.get", spec.ready);
    fake.reply("agent.prompt", prompted_kid("working", 7));
    fake.on_after("agent.get", "agent.prompt", after_prompt);
    fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Hold,
        },
    );
    fake.reply_seq("pane.read", vec![read_result("all done")]);
}

fn prompted_kid(status: &str, seq: u64) -> Value {
    json!({"type": "agent_prompted", "agent": kid_json(status, seq, None, true, false)})
}

fn ready_codex() -> Vec<Value> {
    vec![
        kid("unknown", 1, false, true),
        kid("unknown", 2, true, false),
    ]
}

fn ready_idle() -> Vec<Value> {
    vec![kid("idle", 2, true, false)]
}

fn start_params(h: &Harness) -> Value {
    requests(&h.fake, "agent.start")
        .last()
        .expect("agent.start")["params"]
        .clone()
}

fn assert_no_cleanup(fake: &FakeHerdr) {
    for method in ["pane.close", "tab.close", "workspace.close"] {
        assert_eq!(count(fake, method), 0, "{method}");
    }
}

fn index_of(fake: &FakeHerdr, method: &str) -> usize {
    fake.requests()
        .iter()
        .position(|r| r["method"] == method)
        .unwrap_or_else(|| panic!("no {method} request"))
}

#[tokio::test]
async fn launches_codex_in_a_new_tab_retries_a_busy_start_and_waits_for_the_turn() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 1,
            ready: ready_codex(),
        },
    );

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "review the diff", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["phase"], "waiting");
    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "done");
    assert_eq!(out["pane"], "w1:p9");
    assert_eq!(out["created"], json!({"kind": "tab", "id": "w1:t9"}));
    assert_eq!(out["turn"]["after"], 7);
    assert_eq!(out["turn"]["from"], "working");
    assert_eq!(count(&h.fake, "agent.start"), 2);
    let params = start_params(&h);
    assert_eq!(params["name"], "kid");
    assert_eq!(params["kind"], "codex");
    assert_eq!(params["pane_id"], "w1:p9");
    let prompt = &requests(&h.fake, "agent.prompt")[0]["params"];
    assert_eq!(prompt["text"], "review the diff");
    assert_eq!(
        prompt["wait"],
        json!({"until": ["working", "blocked"], "timeout_ms": 6000})
    );
    assert!(index_of(&h.fake, "pane.report_metadata") < index_of(&h.fake, "agent.start"));
    assert_no_cleanup(&h.fake);
}

fn tokens_of(h: &Harness) -> Vec<(String, Value)> {
    requests(&h.fake, "pane.report_metadata")
        .into_iter()
        .map(|r| {
            (
                r["params"]["pane_id"].as_str().unwrap().to_string(),
                r["params"]["tokens"].clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_launch_into_an_existing_pane_tags_only_the_agent_parent() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p2"}),
        process_info("w1:p2", 200, 200),
    );

    ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "where": {"pane": "w1:p2"}}),
    )
    .await;

    assert_eq!(
        tokens_of(&h),
        vec![("w1:p2".to_string(), json!({ AGENT_PARENT: "w1:p1" }))]
    );
    assert!(index_of(&h.fake, "agent.start") < index_of(&h.fake, "pane.report_metadata"));
    assert_eq!(count(&h.fake, "tab.create"), 0);
}

#[tokio::test]
async fn a_launch_into_a_new_pane_tags_the_agent_parent_too() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );

    ok(&h, "herdr_launch", json!({"kind": "codex", "name": "kid"})).await;

    assert_eq!(
        tokens_of(&h),
        vec![
            ("w1:p9".to_string(), json!({ PARENT: "w1:p1" })),
            ("w1:p9".to_string(), json!({ AGENT_PARENT: "w1:p1" })),
        ]
    );
}

fn blocked_with_tokens(tokens: Value) -> Value {
    let mut agent = rev_with("blocked", 3, None);
    agent["agent"]["tokens"] = tokens;
    agent
}

#[tokio::test]
async fn answer_blocked_follows_the_agent_parent_before_the_pane_parent() {
    let allowed = blocked_with_tokens(json!({ AGENT_PARENT: "w1:p1", PARENT: "w1:p2" }));
    let denied = blocked_with_tokens(json!({ AGENT_PARENT: "w1:p2", PARENT: "w1:p1" }));
    let only_agent = blocked_with_tokens(json!({ AGENT_PARENT: "w1:p1" }));

    for (agent, ok_expected) in [(allowed, true), (only_agent, true), (denied, false)] {
        let h = start(Opts::default()).await;
        script_send(&h.fake, None, vec![agent]);
        let args = json!({"target": "rev", "keys": ["y"], "answer_blocked": true});

        if ok_expected {
            assert_eq!(ok(&h, "herdr_send", args).await["result"], "answered");
        } else {
            assert_eq!(err_code(&h, "herdr_send", args).await, "answer_refused");
            assert_nothing_typed(&h.fake);
        }
    }
}

#[tokio::test]
async fn claude_is_ready_at_idle_with_interactive_ready_and_returns_the_turn_without_waiting() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["phase"], "prompted");
    assert_eq!(out["turn"]["after"], 7);
    assert_eq!(out["turn"]["terminal_id"], "t-w1:p9");
    assert_eq!(out["turn"]["shell_pid"], 200);
    assert_eq!(start_params(&h)["kind"], "claude");
    assert_eq!(count(&h.fake, "events.subscribe"), 0);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn without_instructions_the_launch_stops_at_started() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );

    let out = ok(&h, "herdr_launch", json!({"kind": "claude", "name": "kid"})).await;

    assert_eq!(out["phase"], "started");
    assert!(out["turn"].is_null());
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn a_live_name_is_refused_before_anything_is_created() {
    let h = start(Opts::default()).await;
    caller_ok(&h.fake);
    h.fake
        .reply_seq("session.snapshot", vec![snapshot(false, Some("w1:p1"))]);

    let code = err_code(&h, "herdr_launch", json!({"kind": "codex", "name": "rev"})).await;

    assert_eq!(code, "name_taken");
    for method in [
        "tab.create",
        "pane.split",
        "workspace.create",
        "agent.start",
    ] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn invalid_params_fail_before_any_herdr_call() {
    let h = start(Opts::default()).await;
    for args in [
        json!({"kind": "codex", "name": ""}),
        json!({"kind": "", "name": "kid"}),
        json!({"kind": "codex", "name": "kid", "where": {"tab": true, "workspace": true}}),
        json!({"kind": "codex", "name": "kid", "wait": "turn"}),
    ] {
        assert_eq!(err_code(&h, "herdr_launch", args).await, "invalid_params");
    }
    assert!(h.fake.requests().is_empty());
}

#[tokio::test]
async fn a_blocked_startup_is_agent_not_ready_and_keeps_the_pane() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![
                kid("unknown", 1, false, true),
                kid("blocked", 2, false, false),
            ],
        },
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "agent_not_ready");
    assert_eq!(out["phase"], "started");
    assert_eq!(out["pane"], "w1:p9");
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn an_agent_that_exits_immediately_fails_fast_and_closes_the_tab_it_created() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![kid("unknown", 1, false, true)],
        },
    );
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    h.fake.reply("tab.close", ok_reply());

    let started = Instant::now();
    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go", "start_timeout_ms": 20000}),
    )
    .await;

    assert_eq!(out["code"], "agent_exited");
    assert!(started.elapsed() < Duration::from_secs(5));
    let closed = requests(&h.fake, "tab.close");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["params"]["tab_id"], "w1:t9");
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
}

fn pending() -> Value {
    kid("unknown", 1, false, true)
}

#[tokio::test]
async fn a_shell_still_in_front_right_after_start_is_not_an_exit() {
    let h = start(Opts::default()).await;
    let mut ready = vec![pending(); 5];
    ready.push(kid("idle", 2, true, false));
    script_launch(&h.fake, Launch { busy: 0, ready });
    h.fake.reply_seq(
        "pane.process_info",
        vec![
            process_info("w1:p9", 200, 200),
            process_info("w1:p9", 200, 200),
            process_info("w1:p9", 200, 300),
        ],
    );

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn the_shell_returning_after_the_agent_was_seen_is_agent_exited() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![pending()],
        },
    );
    h.fake.reply_seq(
        "pane.process_info",
        vec![
            process_info("w1:p9", 200, 300),
            process_info("w1:p9", 200, 200),
        ],
    );
    h.fake.reply("tab.close", ok_reply());

    let code = err_code(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "start_timeout_ms": 20000}),
    )
    .await;

    assert_eq!(code, "agent_exited");
    assert_eq!(count(&h.fake, "tab.close"), 1);
}

#[tokio::test]
async fn a_shell_in_front_for_longer_than_the_grace_with_nothing_seen_is_agent_exited() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![pending()],
        },
    );
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    h.fake.reply("tab.close", ok_reply());

    let started = Instant::now();
    let code = err_code(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "start_timeout_ms": 20000}),
    )
    .await;

    assert_eq!(code, "agent_exited");
    assert!(started.elapsed() >= launch_tuning().startup_grace);
    assert_eq!(count(&h.fake, "tab.close"), 1);
}

#[tokio::test]
async fn a_readiness_timeout_ends_with_one_agent_get_and_keeps_the_pane() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![kid("unknown", 1, false, true)],
        },
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go", "start_timeout_ms": 60}),
    )
    .await;

    assert_eq!(out["code"], "agent_not_ready");
    assert_eq!(out["phase"], "started");
    assert_eq!(h.fake.last_request()["method"], "agent.get");
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn the_prompt_waits_for_a_quiet_window_that_restarts_on_every_state_change() {
    let h = start(Opts {
        quiet: QUIET,
        ..Opts::default()
    })
    .await;
    let mut ready: Vec<Value> = vec![kid("unknown", 1, false, true)];
    ready.extend((0..8).map(|_| kid("idle", 3, true, false)));
    ready.push(kid("working", 4, false, false));
    ready.push(kid("done", 5, true, false));
    script_launch(&h.fake, Launch { busy: 0, ready });
    ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "start_timeout_ms": 10000}),
    )
    .await;

    let times = h.fake.request_times();
    let all = h.fake.requests();
    let gets: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, r)| r["method"] == "agent.get")
        .map(|(i, _)| i)
        .collect();
    let settled = gets[10];
    let prompt = index_of(&h.fake, "agent.prompt");
    assert!(
        times[prompt] - times[settled] >= QUIET - Duration::from_millis(3),
        "prompted {:?} after the last state change",
        times[prompt] - times[settled]
    );
}

#[tokio::test]
async fn a_start_that_never_stops_being_busy_closes_the_tab_and_reports_it() {
    let h = start(Opts::default()).await;
    caller_ok(&h.fake);
    h.fake.reply_seq(
        "session.snapshot",
        vec![snapshot(false, None), snapshot(true, None)],
    );
    h.fake.reply("tab.create", tab_created());
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.on_seq(
        "agent.start",
        vec![Script::error("agent_pane_busy", "busy")],
    );
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    h.fake.reply("tab.close", ok_reply());

    let code = err_code(&h, "herdr_launch", json!({"kind": "codex", "name": "kid"})).await;

    assert_eq!(code, "agent_pane_busy");
    assert!(count(&h.fake, "agent.start") > 2);
    assert_eq!(count(&h.fake, "tab.close"), 1);
}

#[tokio::test]
async fn a_start_failure_other_than_busy_closes_the_tab_without_retrying() {
    let h = start(Opts::default()).await;
    caller_ok(&h.fake);
    h.fake.reply_seq(
        "session.snapshot",
        vec![snapshot(false, None), snapshot(true, None)],
    );
    h.fake.reply("tab.create", tab_created());
    h.fake.reply("pane.report_metadata", ok_reply());
    h.fake.on_seq(
        "agent.start",
        vec![Script::error("agent_name_taken", "taken")],
    );
    h.fake.reply("tab.close", ok_reply());

    let code = err_code(&h, "herdr_launch", json!({"kind": "codex", "name": "kid"})).await;

    assert_eq!(code, "agent_name_taken");
    assert_eq!(count(&h.fake, "agent.start"), 1);
    assert_eq!(count(&h.fake, "tab.close"), 1);
}

fn claude_mcp(start: &Value) -> Value {
    let args = start["args"].as_array().expect("args");
    assert_eq!(args.len(), 2, "{args:?}");
    assert_eq!(args[0], "--mcp-config");
    serde_json::from_str(args[1].as_str().unwrap()).unwrap()
}

async fn launch_args(sandbox: Option<InstallEnv>, kind: &str, extra: Value) -> Value {
    let h = start(Opts {
        install_env: sandbox,
        ..Opts::default()
    })
    .await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let mut args = json!({"kind": kind, "name": "kid"});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    ok(&h, "herdr_launch", args).await;
    start_params(&h)
}

#[tokio::test]
async fn claude_gets_a_per_launch_mcp_config_in_read_mode_by_default() {
    let sandbox = Sandbox::new();
    sandbox.link_shim();
    let params = launch_args(Some(sandbox.env.clone()), "claude", json!({})).await;

    let config = claude_mcp(&params);
    assert_eq!(config["mcpServers"]["herdr"]["command"], sandbox.bin());
    assert!(sandbox.bin().ends_with(".local/bin/herdr-mcp"));
    assert_eq!(
        config["mcpServers"]["herdr"]["args"],
        json!(["--client", "claude", "--tools", "read"])
    );
}

#[tokio::test]
async fn the_per_launch_command_falls_back_to_the_binary_when_there_is_no_shim() {
    let sandbox = Sandbox::new();
    let params = launch_args(Some(sandbox.env.clone()), "claude", json!({})).await;

    let config = claude_mcp(&params);
    assert_eq!(
        config["mcpServers"]["herdr"]["command"],
        sandbox.env.exe.display().to_string()
    );
}

#[tokio::test]
async fn codex_gets_one_argv_element_per_c_override() {
    let sandbox = Sandbox::new();
    let params = launch_args(Some(sandbox.env.clone()), "codex", json!({})).await;

    let bin = json!(sandbox.bin());
    assert_eq!(
        params["args"],
        json!([
            "-c",
            format!("mcp_servers.herdr.command={bin}"),
            "-c",
            r#"mcp_servers.herdr.args=["--client","codex","--tools","read"]"#,
            "-c",
            r#"mcp_servers.herdr.env_vars=["HERDR_SOCKET_PATH","HERDR_PANE_ID","HERDR_TAB_ID","HERDR_WORKSPACE_ID","HERDR_ENV","HERDR_SESSION","XDG_CONFIG_HOME"]"#,
            "-c",
            "mcp_servers.herdr.tool_timeout_sec=330",
        ])
    );
}

#[tokio::test]
async fn control_true_from_a_control_mode_caller_injects_control_mode() {
    let sandbox = Sandbox::new();
    let params = launch_args(
        Some(sandbox.env.clone()),
        "claude",
        json!({"control": true}),
    )
    .await;

    assert_eq!(
        claude_mcp(&params)["mcpServers"]["herdr"]["args"],
        json!(["--client", "claude", "--tools", "control"])
    );
}

#[tokio::test]
async fn an_entry_that_is_already_ours_gets_no_per_launch_args() {
    let sandbox = Sandbox::new();
    sandbox.claude_entry_is_ours();
    let params = launch_args(Some(sandbox.env.clone()), "claude", json!({})).await;

    assert!(
        params
            .get("args")
            .is_none_or(|args| args.as_array().unwrap().is_empty())
    );
}

#[tokio::test]
async fn an_unsupported_kind_gets_no_per_launch_args() {
    let sandbox = Sandbox::new();
    let params = launch_args(Some(sandbox.env.clone()), "opencode", json!({})).await;

    assert!(
        params
            .get("args")
            .is_none_or(|args| args.as_array().unwrap().is_empty())
    );
}

#[tokio::test]
async fn without_an_install_env_every_supported_kind_is_injectable() {
    let params = launch_args(None, "claude", json!({})).await;

    assert_eq!(params["args"][0], "--mcp-config");
}

async fn launch_both(sandbox: &Sandbox, kind: &str, control: bool) -> (Value, Value) {
    let h = start(Opts {
        install_env: Some(sandbox.env.clone()),
        ..Opts::default()
    })
    .await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": kind, "name": "kid", "control": control}),
    )
    .await;
    (out, start_params(&h))
}

fn no_args(params: &Value) -> bool {
    params
        .get("args")
        .is_none_or(|args| args.as_array().unwrap().is_empty())
}

#[tokio::test]
async fn codex_with_a_global_entry_in_another_mode_gets_a_tools_override() {
    for (global, control) in [(ToolMode::Control, false), (ToolMode::Read, true)] {
        let sandbox = Sandbox::new();
        sandbox.codex_entry(global);
        let requested = ToolMode::from_control(control).as_str();

        let (out, params) = launch_both(&sandbox, "codex", control).await;

        assert_eq!(out["mcp"], "global");
        assert_eq!(out["child_tools"], requested);
        assert!(out.get("note").is_none(), "{out}");
        assert_eq!(
            params["args"],
            json!([
                "-c",
                format!(r#"mcp_servers.herdr.args=["--client","codex","--tools","{requested}"]"#)
            ])
        );
    }
}

#[tokio::test]
async fn codex_with_a_global_entry_in_the_requested_mode_gets_no_args() {
    for mode in [ToolMode::Read, ToolMode::Control] {
        let sandbox = Sandbox::new();
        sandbox.codex_entry(mode);

        let (out, params) = launch_both(&sandbox, "codex", mode == ToolMode::Control).await;

        assert_eq!(out["mcp"], "global");
        assert_eq!(out["child_tools"], mode.as_str());
        assert!(no_args(&params), "{params}");
    }
}

#[tokio::test]
async fn claude_reports_the_global_mode_and_notes_a_mismatch() {
    for (global, control, differs) in [
        (ToolMode::Control, false, true),
        (ToolMode::Read, true, true),
        (ToolMode::Read, false, false),
        (ToolMode::Control, true, false),
    ] {
        let sandbox = Sandbox::new();
        sandbox.claude_entry(global);

        let (out, params) = launch_both(&sandbox, "claude", control).await;

        assert_eq!(out["mcp"], "global");
        assert_eq!(out["child_tools"], global.as_str());
        assert!(no_args(&params), "{params}");
        assert_eq!(out.get("note").is_some(), differs, "{out}");
        if differs {
            let note = out["note"].as_str().unwrap();
            assert!(note.contains(global.as_str()), "{note}");
            assert!(
                note.contains(ToolMode::from_control(control).as_str()),
                "{note}"
            );
        }
    }
}

#[tokio::test]
async fn an_injected_launch_reports_the_injected_mode_as_child_tools() {
    for control in [false, true] {
        let sandbox = Sandbox::new();
        let (out, _) = launch_both(&sandbox, "claude", control).await;

        assert_eq!(out["mcp"], "injected");
        assert_eq!(out["child_tools"], ToolMode::from_control(control).as_str());
    }
}

#[tokio::test]
async fn a_kind_without_mcp_reports_read_child_tools() {
    let sandbox = Sandbox::new();
    let (out, _) = launch_both(&sandbox, "opencode", false).await;

    assert_eq!(out["mcp"], "unsupported");
    assert_eq!(out["child_tools"], "read");
}

#[tokio::test]
async fn control_true_from_a_read_mode_caller_is_refused_before_anything_is_created() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = direct_inner(&fake, ToolMode::Read, &log_dir);
    let params =
        serde_json::from_value(json!({"kind": "claude", "name": "kid", "control": true})).unwrap();

    let err = launch::run(&inner, None, CancellationToken::new(), None, params)
        .await
        .unwrap_err();

    assert_eq!(err.code(), "control_refused");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn cancel_during_the_wait_reports_phase_waiting_and_leaves_the_pane_and_prompt() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = direct_inner(&fake, ToolMode::Control, &log_dir);
    script_launch_then(
        &fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
        kid("working", 7, true, false),
    );
    let cancel = CancellationToken::new();
    let params: launch::LaunchParams = serde_json::from_value(
        json!({"kind": "claude", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .unwrap();
    let canceller = cancel.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        canceller.cancel();
    });

    let result: Result<Value, HerdrError> = launch::run(&inner, None, cancel, None, params).await;
    task.await.unwrap();

    let err = result.unwrap_err();
    assert_eq!(err.code(), "cancelled");
    let details = err.details().expect("details");
    assert_eq!(details["phase"], "waiting");
    assert_eq!(details["pane"], "w1:p9");
    assert_eq!(details["turn"]["after"], 7);
    assert_eq!(count(&fake, "agent.prompt"), 1);
    assert_no_cleanup(&fake);
}

fn capped_inner(fake: &FakeHerdr, log_dir: &TempDir, cap_ms: u64) -> Inner {
    let mut inner = direct_inner(fake, ToolMode::Control, log_dir);
    inner.profile = Profile::new(None, Some(Duration::from_millis(cap_ms)));
    inner
}

fn launch_params(extra: Value) -> launch::LaunchParams {
    let mut args = json!({"kind": "claude", "name": "kid"});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    serde_json::from_value(args).unwrap()
}

#[tokio::test]
async fn a_start_timeout_above_the_wait_cap_is_capped_and_noted() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = capped_inner(&fake, &log_dir, 300);
    script_launch(
        &fake,
        Launch {
            busy: 0,
            ready: vec![kid("unknown", 1, false, true)],
        },
    );

    let began = Instant::now();
    let err = launch::run(
        &inner,
        None,
        CancellationToken::new(),
        None,
        launch_params(json!({"start_timeout_ms": 2000})),
    )
    .await
    .unwrap_err();

    assert_eq!(err.code(), "agent_not_ready");
    assert!(err.to_string().contains("300 ms"), "{err}");
    assert!(began.elapsed() < Duration::from_millis(1500));

    let fake = FakeHerdr::start().await;
    let inner = capped_inner(&fake, &log_dir, 300);
    script_launch(
        &fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let out = launch::run(
        &inner,
        None,
        CancellationToken::new(),
        None,
        launch_params(json!({"start_timeout_ms": 2000})),
    )
    .await
    .unwrap();
    assert!(out["note"].as_str().unwrap().contains("capped"), "{out}");
}

#[tokio::test]
async fn a_slow_start_leaves_the_turn_wait_only_what_is_left_of_the_cap() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = capped_inner(&fake, &log_dir, 1000);
    script_launch_then(
        &fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
        kid("working", 7, true, false),
    );
    fake.on_seq(
        "agent.start",
        vec![Script::Delayed(
            Duration::from_millis(400),
            Box::new(Script::Reply(started())),
        )],
    );

    let began = Instant::now();
    let out = launch::run(
        &inner,
        None,
        CancellationToken::new(),
        None,
        launch_params(json!({"instructions": "go", "wait": "turn"})),
    )
    .await
    .unwrap();

    assert_eq!(out["timed_out"], true, "{out}");
    assert!(
        began.elapsed() < Duration::from_millis(1250),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn nothing_left_of_the_cap_returns_the_turn_without_waiting() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = capped_inner(&fake, &log_dir, 300);
    script_launch_then(
        &fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
        kid("working", 7, true, false),
    );
    fake.on_seq(
        "agent.start",
        vec![Script::Delayed(
            Duration::from_millis(400),
            Box::new(Script::Reply(started())),
        )],
    );

    let out = launch::run(
        &inner,
        None,
        CancellationToken::new(),
        None,
        launch_params(json!({"instructions": "go", "wait": "turn"})),
    )
    .await
    .unwrap();

    assert_eq!(out["timed_out"], true, "{out}");
    assert_eq!(out["phase"], "waiting");
    assert_eq!(out["turn"]["after"], 7);
    assert_eq!(count(&fake, "events.subscribe"), 0);
    assert_eq!(count(&fake, "agent.prompt"), 1);
}

#[tokio::test]
async fn cancel_during_readiness_keeps_the_pane_and_reports_it() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = direct_inner(&fake, ToolMode::Control, &log_dir);
    script_launch(
        &fake,
        Launch {
            busy: 0,
            ready: vec![kid("unknown", 1, false, true)],
        },
    );
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });

    let began = Instant::now();
    let result = launch::run(&inner, None, cancel, None, launch_params(json!({}))).await;
    task.await.unwrap();

    let err = result.unwrap_err();
    assert_eq!(err.code(), "cancelled");
    let details = err.details().expect("details");
    assert_eq!(details["phase"], "started");
    assert_eq!(details["pane"], "w1:p9");
    assert!(began.elapsed() < Duration::from_secs(2));
    assert_eq!(count(&fake, "agent.prompt"), 0);
    assert_no_cleanup(&fake);
}

#[tokio::test]
async fn a_stalled_prompt_is_surfaced_with_advice_and_phase_prompted() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.on_seq(
        "agent.prompt",
        vec![Script::error(
            "agent_prompt_stalled",
            "no activity after prompt",
        )],
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["code"], "agent_prompt_stalled");
    assert_eq!(out["phase"], "prompted");
    assert_eq!(out["pane"], "w1:p9");
    assert!(out["message"].as_str().unwrap().contains("read"));
    assert_eq!(count(&h.fake, "agent.prompt"), 1);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn an_agent_that_blocks_on_the_first_prompt_returns_the_blocked_result_without_waiting() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.on_seq(
        "agent.prompt",
        vec![Script::Reply(prompted_kid("blocked", 7))],
    );

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["status"], "blocked");
    assert_eq!(out["matched"], true);
    assert_eq!(out["turn"]["from"], "blocked");
    assert_eq!(count(&h.fake, "events.subscribe"), 0);
}

const HOOK_DIALOG: &str = "  Hooks need review\n  1 hook is new or changed.\n  Hooks can run outside the sandbox after you trust them.\n> 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting (hooks won't run)\n  enter confirm · esc skip\n";
const DIR_DIALOG: &str =
    "  Do you trust the contents of this directory?\n> 1. Yes, continue\n  2. No, quit\n";
const CLEAR_SCREEN: &str = "  OpenAI Codex\n> ";

fn screens(fake: &FakeHerdr, texts: &[&str]) {
    fake.reply_seq("agent.read", texts.iter().map(|t| read_result(t)).collect());
}

fn typed_keys(h: &Harness) -> Vec<Value> {
    requests(&h.fake, "agent.send_keys")
        .into_iter()
        .map(|r| r["params"]["keys"].clone())
        .collect()
}

fn launch_with_dialog(h: &Harness, texts: &[&str]) {
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.reply_seq("agent.send_keys", vec![ok_reply()]);
    screens(&h.fake, texts);
}

#[tokio::test]
async fn a_codex_hook_review_dialog_is_skipped_with_esc_before_the_prompt() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[HOOK_DIALOG, CLEAR_SCREEN]);

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(typed_keys(&h), vec![json!(["esc"])]);
    assert_eq!(
        requests(&h.fake, "agent.read")[0]["params"]["source"],
        "visible"
    );
    assert_eq!(out["dialogs"][0]["id"], "hook_review");
    assert_eq!(out["dialogs"][0]["action"], "skipped");
    assert_eq!(out["dialogs"][0]["verified"], true);
    assert!(
        out["note"]
            .as_str()
            .unwrap()
            .contains("Codex hooks were skipped for this child")
    );
    assert_eq!(count(&h.fake, "agent.prompt"), 1);
    assert!(index_of(&h.fake, "agent.send_keys") < index_of(&h.fake, "agent.prompt"));
}

#[tokio::test]
async fn dialogs_surface_stops_with_agent_needs_input_and_types_nothing() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[HOOK_DIALOG]);

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "dialogs": "surface"}),
    )
    .await;

    assert_eq!(out["code"], "agent_needs_input");
    assert_eq!(out["phase"], "started");
    assert_eq!(out["pane"], "w1:p9");
    assert_eq!(out["name"], "kid");
    assert_eq!(out["dialog"]["id"], "hook_review");
    assert!(
        out["dialog"]["excerpt"]
            .as_str()
            .unwrap()
            .contains("Hooks need review")
    );
    assert!(typed_keys(&h).is_empty());
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn dialogs_trust_sends_the_trust_keys() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[HOOK_DIALOG, CLEAR_SCREEN]);

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "dialogs": "trust"}),
    )
    .await;

    assert_eq!(typed_keys(&h), vec![json!(["2", "enter"])]);
    assert_eq!(out["dialogs"][0]["action"], "trusted");
    assert!(out.get("note").is_none_or(|n| n.is_null()));
}

#[tokio::test]
async fn a_dialog_that_survives_the_skip_is_agent_needs_input() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[HOOK_DIALOG]);

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "agent_needs_input");
    assert_eq!(out["dialog"]["id"], "hook_review");
    assert_eq!(typed_keys(&h), vec![json!(["esc"])]);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn chained_dialogs_are_each_handled() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[HOOK_DIALOG, DIR_DIALOG, CLEAR_SCREEN]);

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(typed_keys(&h), vec![json!(["esc"]), json!(["esc"])]);
    assert_eq!(out["dialogs"][0]["id"], "hook_review");
    assert_eq!(out["dialogs"][1]["id"], "directory_trust");
    assert_eq!(out["dialogs"][1]["verified"], false);
}

#[tokio::test]
async fn a_launch_with_no_dialog_types_nothing_extra() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[CLEAR_SCREEN]);

    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert!(typed_keys(&h).is_empty());
    assert!(out.get("dialogs").is_none_or(|d| d.is_null()));
    assert_eq!(count(&h.fake, "agent.prompt"), 1);
}

#[tokio::test]
async fn answer_blocked_on_an_idle_own_child_with_a_dialog_on_screen_sends_the_keys() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);
    screens(&h.fake, &[HOOK_DIALOG]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["esc"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(out["result"], "answered");
    assert_eq!(typed_keys(&h), vec![json!(["esc"])]);
}

#[tokio::test]
async fn answer_blocked_on_an_idle_own_child_with_no_dialog_is_not_blocked() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);
    screens(&h.fake, &[CLEAR_SCREEN]);

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["esc"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(code, "not_blocked");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn answer_blocked_on_an_idle_child_of_another_caller_is_refused_even_with_a_dialog() {
    let h = start(Opts::default()).await;
    script_send(
        &h.fake,
        Some("w1:p2"),
        vec![rev_with("idle", 3, Some("w1:p2"))],
    );
    screens(&h.fake, &[HOOK_DIALOG]);

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["esc"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(code, "answer_refused");
    assert_nothing_typed(&h.fake);
}

fn script_send(fake: &FakeHerdr, rev_tokens: Option<&'static str>, gets: Vec<Value>) {
    caller_ok(fake);
    fake.reply_seq("session.snapshot", vec![snapshot(false, rev_tokens)]);
    fake.reply_seq("agent.get", gets);
    fake.reply_seq("pane.process_info", vec![process_info("w1:p4", 300, 300)]);
    fake.reply("agent.prompt", prompted("working", 5));
    fake.reply_seq("agent.send_keys", vec![ok_reply()]);
    fake.reply_seq("pane.send_keys", vec![ok_reply()]);
    fake.reply_seq("pane.send_text", vec![ok_reply()]);
    fake.reply_seq("pane.send_input", vec![ok_reply()]);
}

fn assert_nothing_typed(fake: &FakeHerdr) {
    for method in [
        "agent.prompt",
        "agent.send_keys",
        "pane.send_keys",
        "pane.send_text",
        "pane.send_input",
    ] {
        assert_eq!(count(fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn auto_to_an_idle_agent_starts_a_turn() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next task"}),
    )
    .await;

    assert_eq!(out["result"], "started");
    assert_eq!(out["pane"], "w1:p4");
    assert_eq!(out["turn"]["after"], 5);
    assert_eq!(out["turn"]["from"], "working");
    assert_eq!(out["turn"]["terminal_id"], "t-w1:p4");
    let prompt = &requests(&h.fake, "agent.prompt")[0]["params"];
    assert_eq!(prompt["text"], "next task");
    assert_eq!(
        prompt["wait"],
        json!({"until": ["working", "blocked"], "timeout_ms": 6000})
    );
}

#[tokio::test]
async fn auto_to_a_working_claude_queues_the_prompt() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev_kind("working", "claude")]);
    screens(&h.fake, &[QUEUED_SCREEN]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next task"}),
    )
    .await;

    assert_eq!(out["mode"], "auto");
    assert_eq!(out["result"], "queued");
    assert_eq!(out["delivery"], "queued");
    assert!(out["turn"].is_null());
    assert_eq!(count(&h.fake, "agent.prompt"), 1);
}

#[tokio::test]
async fn wait_turn_returns_the_settled_turn_with_its_excerpt() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3), rev("done", 6)]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["delivery"], "started");
    assert_eq!(out["matched"], true);
    assert_eq!(out["timed_out"], false);
    assert_eq!(out["status"], "done");
    assert!(out["excerpt"].is_string(), "{out}");
    assert_eq!(out["turn"]["after"], 5);
    assert_eq!(out["pane"], "w1:p4");
}

#[tokio::test]
async fn a_wait_that_fails_after_delivery_keeps_the_turn_and_says_not_to_resend() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);
    // The prompt lands; then the agent vanishes while the wait polls it.
    h.fake.on_after(
        "agent.get",
        "agent.prompt",
        Script::error("agent_not_found", "gone"),
    );

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["delivery"], "started");
    assert_eq!(out["turn"]["after"], 5);
    assert!(out["wait_error"]["code"].is_string(), "{out}");
    assert!(
        out["note"].as_str().unwrap().contains("do not resend"),
        "{out}"
    );
    assert_eq!(count(&h.fake, "agent.prompt"), 1);
}

#[tokio::test]
async fn the_waits_own_note_survives_wait_turn() {
    let h = start(Opts::default()).await;
    script_send(
        &h.fake,
        Some("w1:p1"),
        vec![rev("idle", 3), rev("unknown", 6)],
    );

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["matched"], true, "{out}");
    assert!(out["note"].as_str().unwrap().contains("unknown"), "{out}");
}

#[tokio::test]
async fn wait_turn_on_a_queued_prompt_returns_at_once() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev_kind("working", "claude")]);
    screens(&h.fake, &[QUEUED_SCREEN]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next task", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["delivery"], "queued");
    assert!(out.get("matched").is_none());
}

#[tokio::test]
async fn wait_is_refused_for_text_and_keys() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "text": "hi", "wait": "turn"}),
    )
    .await;

    assert!(
        out.to_string().contains("wait applies to a prompt"),
        "{out}"
    );
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn auto_to_a_working_codex_is_busy_at_once_and_types_nothing() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("working", 3)]);
    let started = std::time::Instant::now();

    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next", "timeout_ms": 5000}),
    )
    .await;

    assert_eq!(out["code"], "busy");
    assert_eq!(out["kind"], "codex");
    assert!(
        out["message"].as_str().unwrap().contains("after_idle"),
        "{out}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn a_late_queue_marker_still_counts_as_queued() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev_kind("working", "claude")]);
    // The dialog check, then two reads before the marker shows.
    screens(
        &h.fake,
        &[
            WORKING_SCREEN,
            WORKING_SCREEN,
            WORKING_SCREEN,
            QUEUED_SCREEN,
        ],
    );

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next task"}),
    )
    .await;

    assert_eq!(out["delivery"], "queued");
    assert_eq!(out["result"], "queued");
}

#[tokio::test]
async fn a_queue_marker_that_never_shows_is_unconfirmed_with_a_note() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev_kind("working", "claude")]);
    screens(&h.fake, &[WORKING_SCREEN]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next task"}),
    )
    .await;

    assert_eq!(out["delivery"], "unconfirmed");
    assert_eq!(out["result"], "unconfirmed");
    assert!(
        out["note"].as_str().unwrap().contains("not confirmed"),
        "{out}"
    );
}

#[tokio::test]
async fn a_dialog_on_screen_is_refused_before_typing() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev_kind("working", "claude")]);
    screens(
        &h.fake,
        &["Do you trust the files in this folder?\n 1. Yes, proceed\n"],
    );

    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next task"}),
    )
    .await;

    assert_eq!(out["code"], "agent_needs_input");
    assert_eq!(out["dialog"], "folder_trust");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn after_idle_waits_for_idle_then_sends() {
    let h = start(Opts::default()).await;
    script_send(
        &h.fake,
        Some("w1:p1"),
        vec![rev("working", 3), rev("working", 3), rev("done", 4)],
    );

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next", "mode": "after_idle", "timeout_ms": 5000}),
    )
    .await;

    assert_eq!(out["result"], "sent_after_idle");
    assert_eq!(out["turn"]["after"], 5);
    assert!(count(&h.fake, "agent.get") >= 3);
    assert_eq!(count(&h.fake, "agent.prompt"), 1);
}

#[tokio::test]
async fn cancel_while_waiting_for_idle_sends_nothing() {
    for (mode, interrupted) in [("after_idle", false), ("restart", true)] {
        let fake = FakeHerdr::start().await;
        let log_dir = tempfile::tempdir().unwrap();
        let inner = direct_inner(&fake, ToolMode::Control, &log_dir);
        script_send(&fake, Some("w1:p1"), vec![rev("working", 3)]);
        let params: SendParams = serde_json::from_value(
            json!({"target": "rev", "prompt": "next", "mode": mode, "timeout_ms": 4000}),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            canceller.cancel();
        });

        let began = Instant::now();
        let result = send::run(&inner, None, &cancel, None, params).await;
        task.await.unwrap();

        let err = result.unwrap_err();
        assert_eq!(err.code(), "cancelled", "{mode}");
        let details = err.details().expect("details");
        assert_eq!(details["pane"], "w1:p4");
        assert_eq!(details["sent"], false);
        assert_eq!(details["interrupted"], interrupted, "{mode}");
        assert!(began.elapsed() < Duration::from_secs(2), "{mode}");
        assert_eq!(count(&fake, "agent.prompt"), 0, "{mode}");
        assert_eq!(count(&fake, "pane.send_text"), 0, "{mode}");
    }
}

#[tokio::test]
async fn after_idle_that_never_idles_is_not_sent() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("working", 3)]);

    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next", "mode": "after_idle", "timeout_ms": 60}),
    )
    .await;

    assert_eq!(out["code"], "not_sent");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn steer_types_into_a_working_agent_with_no_turn() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("working", 3)]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "also check the tests", "mode": "steer"}),
    )
    .await;

    assert_eq!(out["result"], "steered");
    assert!(out["turn"].is_null());
    let prompt = &requests(&h.fake, "agent.prompt")[0]["params"];
    assert_eq!(prompt["text"], "also check the tests");
    assert!(prompt.get("wait").is_none_or(Value::is_null));
}

fn rev_kind(status: &'static str, kind: &'static str) -> Value {
    json!({"type": "agent_info", "agent": Rev {status, seq: 3, parent: Some("w1:p1"), kind}.json()})
}

const QUEUED_SCREEN: &str =
    "> next task\n\n  Press up to edit queued messages\n  ctrl+enter to send now\n";
const WORKING_SCREEN: &str = "* Pondering... (esc to interrupt)\n> \n";

async fn steer_with(kind: &'static str, screen: &str, extra: Value) -> (Harness, Value) {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev_kind("working", kind)]);
    screens(&h.fake, &[screen]);
    let mut params = json!({"target": "rev", "prompt": "next task", "mode": "steer"});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let out = ok(&h, "herdr_send", params).await;
    (h, out)
}

#[tokio::test]
async fn an_idle_agent_gets_delivery_started() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let out = ok(&h, "herdr_send", json!({"target": "rev", "prompt": "go"})).await;

    assert_eq!(out["delivery"], "started");
    assert!(out["turn"].is_object());
    assert!(out.get("note").is_none());
    assert_eq!(count(&h.fake, "agent.read"), 0);
}

#[tokio::test]
async fn a_working_claude_that_queues_the_prompt_is_delivery_queued_with_a_note() {
    let (h, out) = steer_with("claude", QUEUED_SCREEN, json!({})).await;

    assert_eq!(out["delivery"], "queued");
    assert_eq!(out["result"], "steered");
    assert!(out["turn"].is_null());
    let note = out["note"].as_str().unwrap();
    assert!(note.contains("queued this prompt"), "{note}");
    assert!(note.contains("send_now"), "{note}");
    assert!(typed_keys(&h).is_empty());
}

#[tokio::test]
async fn send_now_presses_ctrl_enter_on_a_queued_claude_prompt() {
    let (h, out) = steer_with("claude", QUEUED_SCREEN, json!({"send_now": true})).await;

    assert_eq!(out["delivery"], "sent_now");
    assert!(out.get("note").is_none());
    assert_eq!(typed_keys(&h), [json!(["ctrl+enter"])]);
    assert!(index_of(&h.fake, "agent.prompt") < index_of(&h.fake, "agent.send_keys"));
}

#[tokio::test]
async fn send_now_without_a_queue_marker_presses_nothing() {
    let (h, out) = steer_with("claude", WORKING_SCREEN, json!({"send_now": true})).await;

    assert_eq!(out["delivery"], "steered");
    assert!(typed_keys(&h).is_empty());
}

#[tokio::test]
async fn a_working_claude_without_the_queue_marker_is_steered() {
    let (_h, out) = steer_with("claude", WORKING_SCREEN, json!({})).await;

    assert_eq!(out["delivery"], "steered");
    assert!(out.get("note").is_none());
}

#[tokio::test]
async fn a_working_codex_is_steered_without_reading_the_screen() {
    let (h, out) = steer_with("codex", QUEUED_SCREEN, json!({})).await;

    assert_eq!(out["delivery"], "steered");
    assert_eq!(count(&h.fake, "agent.read"), 0);
}

#[tokio::test]
async fn send_now_for_a_kind_without_the_key_is_noted_and_presses_nothing() {
    let (h, out) = steer_with("codex", QUEUED_SCREEN, json!({"send_now": true})).await;

    assert_eq!(out["delivery"], "steered");
    assert!(
        out["note"]
            .as_str()
            .unwrap()
            .contains("send_now is not supported for codex")
    );
    assert!(typed_keys(&h).is_empty());
}

#[tokio::test]
async fn send_now_on_an_idle_codex_start_is_noted() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "send_now": true}),
    )
    .await;

    assert_eq!(out["delivery"], "started");
    assert!(out["note"].as_str().unwrap().contains("not supported"));
    assert!(typed_keys(&h).is_empty());
}

#[tokio::test]
async fn send_now_with_raw_text_is_invalid() {
    let h = start(Opts::default()).await;
    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "text": "x", "send_now": true}),
    )
    .await;
    assert!(out.to_string().contains("send_now"));
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn restart_interrupts_waits_for_idle_and_sends() {
    let h = start(Opts::default()).await;
    script_send(
        &h.fake,
        Some("w1:p1"),
        vec![rev("working", 3), rev("working", 3), rev("done", 4)],
    );

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "start over", "mode": "restart", "timeout_ms": 5000}),
    )
    .await;

    assert_eq!(out["result"], "restarted");
    assert_eq!(out["turn"]["after"], 5);
    let keys = requests(&h.fake, "agent.send_keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["params"]["keys"], json!(["esc"]));
    assert!(index_of(&h.fake, "agent.send_keys") < index_of(&h.fake, "agent.prompt"));
}

#[tokio::test]
async fn restart_that_never_reaches_idle_is_not_sent() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("working", 3)]);

    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "prompt": "start over", "mode": "restart", "timeout_ms": 60}),
    )
    .await;

    assert_eq!(out["code"], "not_sent");
    assert_eq!(out["interrupted"], true);
    assert_eq!(count(&h.fake, "agent.send_keys"), 1);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn a_prompt_to_a_blocked_agent_is_agent_blocked() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("blocked", 3)]);

    for mode in ["auto", "after_idle", "steer", "restart"] {
        let code = err_code(
            &h,
            "herdr_send",
            json!({"target": "rev", "prompt": "hi", "mode": mode, "timeout_ms": 50}),
        )
        .await;
        assert_eq!(code, "agent_blocked", "{mode}");
    }
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn a_stalled_prompt_in_send_is_surfaced_with_advice() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);
    h.fake.on_seq(
        "agent.prompt",
        vec![Script::error("agent_prompt_stalled", "no activity")],
    );

    let out = err(&h, "herdr_send", json!({"target": "rev", "prompt": "hi"})).await;

    assert_eq!(out["code"], "agent_prompt_stalled");
    assert_eq!(out["phase"], "prompted");
    assert!(out["message"].as_str().unwrap().contains("read"));
}

#[tokio::test]
async fn keys_to_a_blocked_agent_without_answer_blocked_are_refused() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("blocked", 3)]);

    for args in [
        json!({"target": "rev", "keys": ["y"]}),
        json!({"target": "rev", "text": "y"}),
    ] {
        assert_eq!(err_code(&h, "herdr_send", args).await, "answer_refused");
    }
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn answer_blocked_reaches_a_child_this_caller_launched() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("blocked", 3)]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["y"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(out["result"], "answered");
    let sent = requests(&h.fake, "agent.send_keys");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["params"]["target"], "w1:p4");
    assert_eq!(sent[0]["params"]["keys"], json!(["y"]));
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn answer_blocked_text_goes_through_send_text() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("blocked", 3)]);

    ok(
        &h,
        "herdr_send",
        json!({"target": "rev", "text": "yes", "answer_blocked": true}),
    )
    .await;

    let sent = requests(&h.fake, "pane.send_text");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["params"]["pane_id"], "w1:p4");
    assert_eq!(sent[0]["params"]["text"], "yes");
}

#[tokio::test]
async fn answer_blocked_is_refused_for_a_human_launched_pane() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, None, vec![rev_with("blocked", 3, None)]);

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["y"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(code, "answer_refused");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn answer_blocked_is_refused_for_another_agents_child() {
    let h = start(Opts::default()).await;
    script_send(
        &h.fake,
        Some("w1:p2"),
        vec![rev_with("blocked", 3, Some("w1:p2"))],
    );

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["y"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(code, "answer_refused");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn answer_blocked_is_refused_for_an_unverified_caller() {
    let h = start(Opts::default()).await;
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 777, 777),
    );
    h.fake
        .reply_seq("session.snapshot", vec![snapshot(false, Some("w1:p1"))]);
    h.fake
        .reply_seq("agent.get", vec![rev_with("blocked", 3, Some("w1:p1"))]);
    h.fake.reply_seq("agent.send_keys", vec![ok_reply()]);

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["y"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(code, "answer_refused");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn answer_blocked_on_an_agent_that_is_not_blocked_types_nothing() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("working", 3)]);

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["y"], "answer_blocked": true}),
    )
    .await;

    assert_eq!(code, "not_blocked");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn text_and_keys_to_a_plain_pane_are_typed_without_a_prompt() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let out = ok(
        &h,
        "herdr_send",
        json!({"target": "w1:p3", "text": "ls", "keys": ["Enter"]}),
    )
    .await;

    assert_eq!(out["result"], "sent");
    assert_eq!(out["text_sent"], true);
    let sent = requests(&h.fake, "pane.send_input");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["params"],
        json!({"pane_id": "w1:p3", "text": "ls", "keys": ["Enter"]})
    );
    assert_eq!(count(&h.fake, "pane.send_text"), 0);
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn a_rejected_key_with_text_to_a_plain_pane_types_nothing() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);
    h.fake.on_seq(
        "pane.send_input",
        vec![Script::error("invalid_key", "unsupported key zz")],
    );

    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "w1:p3", "text": "ls", "keys": ["zz"]}),
    )
    .await;

    assert_eq!(code, "invalid_key");
    assert_eq!(count(&h.fake, "pane.send_text"), 0);
    assert_eq!(count(&h.fake, "pane.send_keys"), 0);
}

#[tokio::test]
async fn a_keys_failure_after_text_to_an_agent_reports_the_text_as_sent() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);
    h.fake.on_seq(
        "agent.send_keys",
        vec![Script::error("invalid_key", "unsupported key zz")],
    );

    let out = err(
        &h,
        "herdr_send",
        json!({"target": "rev", "text": "ls", "keys": ["zz"]}),
    )
    .await;

    assert_eq!(out["code"], "keys_failed");
    assert_eq!(out["text_sent"], true);
    assert_eq!(out["pane"], "w1:p4");
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .contains("unsupported key zz")
    );
    assert_eq!(count(&h.fake, "pane.send_text"), 1);
    assert_eq!(count(&h.fake, "pane.send_input"), 0);
}

#[tokio::test]
async fn a_prompt_to_a_plain_pane_is_not_an_agent() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let code = err_code(&h, "herdr_send", json!({"target": "w1:p3", "prompt": "hi"})).await;

    assert_eq!(code, "not_an_agent");
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn sending_to_your_own_pane_is_refused() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    for args in [
        json!({"target": "w1:p1", "text": "x"}),
        json!({"target": "w1:p1", "prompt": "x"}),
        json!({"target": "w1:p1", "keys": ["y"], "answer_blocked": true}),
    ] {
        assert_eq!(err_code(&h, "herdr_send", args).await, "self_target");
    }
    assert_nothing_typed(&h.fake);
}

#[tokio::test]
async fn send_params_must_pick_one_form() {
    let h = start(Opts::default()).await;
    script_send(&h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    for args in [
        json!({"target": "rev"}),
        json!({"target": "rev", "prompt": "x", "keys": ["y"]}),
        json!({"target": "rev", "keys": ["y"], "mode": "steer"}),
        json!({"target": "rev", "prompt": ""}),
    ] {
        assert_eq!(err_code(&h, "herdr_send", args).await, "invalid_params");
    }
    assert_nothing_typed(&h.fake);
}

fn agent_for_readiness(status: &str, ready: bool, pending: bool) -> herdr_mcp::herdr::AgentInfo {
    serde_json::from_value(kid_json(status, 1, None, ready, pending)).unwrap()
}

#[test]
fn readiness_follows_ktd7() {
    use herdr_mcp::providers::is_ready;
    let cases = [
        ("claude", "idle", true, true, true),
        ("claude", "done", true, true, true),
        ("claude", "idle", false, false, true),
        ("claude", "idle", false, true, false),
        ("claude", "unknown", true, true, false),
        ("codex", "unknown", true, true, true),
        ("codex", "unknown", false, true, false),
        ("codex", "working", true, false, false),
        ("codex", "blocked", true, false, false),
    ];
    for (kind, status, ready, pending, want) in cases {
        let agent = agent_for_readiness(status, ready, pending);
        assert_eq!(
            is_ready(Some(kind), &agent),
            want,
            "{kind} {status} ready={ready} pending={pending}"
        );
    }
}

// ---- runtime_mode ----

fn flags(start: &Value) -> Vec<String> {
    start["args"]
        .as_array()
        .map(|args| {
            args.iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn has_run(args: &[String], expected: &[&str]) -> bool {
    args.windows(expected.len())
        .any(|w| w.iter().map(String::as_str).eq(expected.iter().copied()))
}

/// The caller pane runs `cmdline` in the foreground; its shell is still the bridge's ancestor.
fn caller_runs(fake: &FakeHerdr, name: &str, argv: &[&str]) {
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        json!({"type": "pane_process_info", "process_info": {
            "pane_id": "w1:p1", "shell_pid": 4242, "foreground_process_group_id": 5000,
            "tty": "/dev/ttys001",
            "foreground_processes": [{"pid": 5000, "name": name, "argv": argv}]}}),
    );
}

async fn launch_with_mode(
    kind: &str,
    extra: Value,
    caller: Option<(&str, &[&str])>,
) -> (Harness, Result<Value, Value>) {
    let h = start(Opts::default()).await;
    if let Some((name, argv)) = caller {
        caller_runs(&h.fake, name, argv);
    }
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let mut args = json!({"kind": kind, "name": "kid"});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    let result = call(&h, "herdr_launch", args).await;
    let out = if result.is_error == Some(true) {
        Err(result.structured_content.unwrap())
    } else {
        Ok(result.structured_content.unwrap())
    };
    (h, out)
}

fn assert_nothing_created(h: &Harness) {
    for method in [
        "tab.create",
        "workspace.create",
        "pane.split",
        "agent.start",
    ] {
        assert_eq!(count(&h.fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn runtime_mode_maps_to_codex_flags() {
    for (mode, expected) in [
        ("approval-required", ["-a", "on-request", "-s", "read-only"]),
        (
            "auto-accept-edits",
            ["-a", "on-request", "-s", "workspace-write"],
        ),
        ("full-access", ["-a", "never", "-s", "danger-full-access"]),
    ] {
        let (h, out) = launch_with_mode(
            "codex",
            json!({"runtime_mode": mode, "allow_full_access": true}),
            None,
        )
        .await;
        let out = out.unwrap();
        let args = flags(&start_params(&h));
        assert!(has_run(&args, &expected), "{mode}: {args:?}");
        assert_eq!(out["runtime_mode"], mode);
        assert_eq!(out["caller_runtime_mode"], "unknown");
    }
}

#[tokio::test]
async fn runtime_mode_maps_to_claude_permission_mode() {
    for (mode, value) in [
        ("approval-required", "manual"),
        ("auto-accept-edits", "acceptEdits"),
        ("full-access", "bypassPermissions"),
    ] {
        let (h, out) = launch_with_mode(
            "claude",
            json!({"runtime_mode": mode, "allow_full_access": true}),
            None,
        )
        .await;
        out.unwrap();
        let args = flags(&start_params(&h));
        assert!(
            has_run(&args, &["--permission-mode", value]),
            "{mode}: {args:?}"
        );
    }
}

#[tokio::test]
async fn inherit_and_an_omitted_runtime_mode_add_no_flags() {
    for extra in [json!({}), json!({"runtime_mode": "inherit"})] {
        let (h, out) = launch_with_mode("codex", extra, None).await;
        let out = out.unwrap();
        let args = flags(&start_params(&h));
        assert!(!args.iter().any(|a| a == "-a" || a == "-s"), "{args:?}");
        assert_eq!(out["runtime_mode"], "inherit");
    }
    let (h, out) = launch_with_mode("claude", json!({}), None).await;
    out.unwrap();
    assert!(!flags(&start_params(&h)).contains(&"--permission-mode".to_string()));
}

#[tokio::test]
async fn runtime_mode_and_the_mcp_args_are_both_present() {
    let (h, out) = launch_with_mode(
        "codex",
        json!({"runtime_mode": "auto-accept-edits", "control": false}),
        None,
    )
    .await;
    out.unwrap();
    let args = flags(&start_params(&h));
    assert!(has_run(
        &args,
        &["-a", "on-request", "-s", "workspace-write"]
    ));
    assert!(
        args.iter()
            .any(|a| a.starts_with("mcp_servers.herdr.command="))
    );

    let (h, out) =
        launch_with_mode("claude", json!({"runtime_mode": "approval-required"}), None).await;
    out.unwrap();
    let args = flags(&start_params(&h));
    assert!(has_run(&args, &["--permission-mode", "manual"]));
    assert!(args.contains(&"--mcp-config".to_string()));
}

#[tokio::test]
async fn a_known_read_only_caller_cannot_launch_auto_accept_edits() {
    let (h, out) = launch_with_mode(
        "claude",
        json!({"runtime_mode": "auto-accept-edits"}),
        Some(("codex", &["codex", "-s", "read-only"])),
    )
    .await;
    let err = out.unwrap_err();
    assert_eq!(err["code"], "runtime_mode_escalation_denied");
    assert_eq!(err["caller_runtime_mode"], "known: approval-required");
    assert_nothing_created(&h);
}

#[tokio::test]
async fn a_known_full_access_caller_may_launch_full_access() {
    for (name, argv) in [
        (
            "codex",
            vec!["codex", "-a", "never", "-s", "danger-full-access"],
        ),
        (
            "codex",
            vec!["codex", "--dangerously-bypass-approvals-and-sandbox"],
        ),
        (
            "claude",
            vec!["claude", "--permission-mode", "bypassPermissions"],
        ),
        ("claude", vec!["claude", "--dangerously-skip-permissions"]),
    ] {
        let (_h, out) = launch_with_mode(
            "codex",
            json!({"runtime_mode": "full-access"}),
            Some((name, &argv)),
        )
        .await;
        let out = out.unwrap();
        assert_eq!(out["caller_runtime_mode"], "known: full-access", "{argv:?}");
    }
}

#[tokio::test]
async fn an_unknown_caller_cannot_launch_full_access_without_the_flag() {
    let (h, out) = launch_with_mode("codex", json!({"runtime_mode": "full-access"}), None).await;
    let err = out.unwrap_err();
    assert_eq!(err["code"], "runtime_mode_escalation_denied");
    assert_eq!(err["caller_runtime_mode"], "unknown");
    assert_nothing_created(&h);
}

#[tokio::test]
async fn an_unknown_caller_may_launch_full_access_with_allow_full_access() {
    let (_h, out) = launch_with_mode(
        "codex",
        json!({"runtime_mode": "full-access", "allow_full_access": true}),
        None,
    )
    .await;
    assert_eq!(out.unwrap()["caller_runtime_mode"], "unknown");
}

#[tokio::test]
async fn an_unknown_caller_may_launch_auto_accept_edits() {
    let (_h, out) =
        launch_with_mode("codex", json!({"runtime_mode": "auto-accept-edits"}), None).await;
    out.unwrap();
}

#[tokio::test]
async fn a_known_caller_is_not_widened_by_allow_full_access() {
    let (h, out) = launch_with_mode(
        "codex",
        json!({"runtime_mode": "full-access", "allow_full_access": true}),
        Some(("claude", &["claude", "--permission-mode", "acceptEdits"])),
    )
    .await;
    assert_eq!(out.unwrap_err()["code"], "runtime_mode_escalation_denied");
    assert_nothing_created(&h);
}

#[tokio::test]
async fn a_kind_without_runtime_flags_refuses_a_runtime_mode() {
    let (h, out) = launch_with_mode(
        "opencode",
        json!({"runtime_mode": "approval-required"}),
        None,
    )
    .await;
    assert_eq!(out.unwrap_err()["code"], "runtime_mode_unsupported");
    assert_nothing_created(&h);

    let (_h, out) = launch_with_mode("opencode", json!({"runtime_mode": "inherit"}), None).await;
    out.unwrap();
}

#[tokio::test]
async fn launch_split_of_a_pane_id_targets_that_pane() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.reply(
        "pane.split",
        json!({"type": "pane_info", "pane": pane("w1:p9", "w1:t1", "w1", None)}),
    );

    ok(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "where": {"split": "down", "of": "w2:p3"}}),
    )
    .await;

    let split = &requests(&h.fake, "pane.split")[0]["params"];
    assert_eq!(split["target_pane_id"], "w2:p3");
    assert_eq!(count(&h.fake, "tab.create"), 0);
}

// ---- reply: "mail" -------------------------------------------------------------------------

use herdr_mcp::mailbox::{Inbox, Mailbox, Message, Party, new_id, now_ms, recipient_key};

const PARAGRAPH_TAIL: &str = ". The body is the deliverable: exact, complete, nothing else. Do this even if you also print it.";

fn paragraph(parent: &str) -> String {
    format!(
        "When you have finished, send your complete final result as the body of one herdr_mail_send call to {parent}{PARAGRAPH_TAIL}"
    )
}

fn mail_from(pane: &str, name: Option<&str>, body: &str) -> Message {
    Message {
        id: new_id(),
        ts_ms: now_ms(),
        from: Party {
            pane: Some(pane.into()),
            name: name.map(Into::into),
        },
        to: Party {
            pane: Some("w1:p1".into()),
            name: None,
        },
        body: body.into(),
        reply_to: None,
    }
}

struct MailEnv {
    h: Harness,
    state: TempDir,
}

impl MailEnv {
    fn mailbox(&self) -> Mailbox {
        Mailbox::new(self.state.path(), self.h.fake.path())
    }

    fn parent_inbox(&self) -> Inbox {
        Inbox::new("w1:p1", None)
    }

    fn unread(&self) -> usize {
        self.mailbox().unread_count(&self.parent_inbox(), None)
    }
}

async fn mail_env(opts: Opts) -> MailEnv {
    let state = tempfile::tempdir().unwrap();
    let h = start(Opts {
        mailbox: Some(state.path().to_path_buf()),
        ..opts
    })
    .await;
    MailEnv { h, state }
}

fn deliver_later(env: &MailEnv, after: Duration, message: Message) -> tokio::task::JoinHandle<()> {
    let mailbox = env.mailbox();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        mailbox.deliver(&recipient_key("w1:p1"), &message).unwrap();
    })
}

fn script_working_child(fake: &FakeHerdr) {
    script_launch_then(
        fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
        kid("working", 7, true, false),
    );
}

fn mail_launch_args(extra: Value) -> Value {
    let mut args = json!({
        "kind": "codex", "name": "kid", "instructions": "go", "wait": "turn", "reply": "mail",
    });
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    args
}

#[tokio::test]
async fn a_blocking_mail_reply_is_exact_consumed_and_leaves_other_mail_alone() {
    let env = mail_env(Opts::default()).await;
    script_working_child(&env.h.fake);
    let other = mail_from("w1:p3", None, "someone else");
    env.mailbox()
        .deliver(&recipient_key("w1:p1"), &other)
        .unwrap();
    let child = mail_from("w1:p9", Some("kid"), "the exact result");
    let sent = deliver_later(&env, Duration::from_millis(300), child.clone());

    let out = ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    sent.await.unwrap();

    assert_eq!(out["reply"]["body"], "the exact result");
    assert_eq!(out["reply"]["id"], child.id);
    assert_eq!(out["reply"]["ts_ms"], child.ts_ms);
    assert_eq!(
        out["reply"]["from"],
        json!({"pane": "w1:p9", "name": "kid"})
    );
    assert!(out.get("excerpt").is_none(), "{out}");
    assert_eq!(out["phase"], "waiting");
    assert_eq!(out["matched"], true);
    assert_eq!(out["timed_out"], false);
    assert!(out["status"].is_string());
    // Only the child's message was consumed.
    assert_eq!(env.unread(), 1);
    let (left, _) = env
        .mailbox()
        .take(&env.parent_inbox(), None, false, 10)
        .unwrap();
    assert_eq!(left[0].body, "someone else");
    assert_no_cleanup(&env.h.fake);
}

#[tokio::test]
async fn a_name_match_alone_is_enough_to_claim_the_reply() {
    let env = mail_env(Opts::default()).await;
    script_working_child(&env.h.fake);
    let msg = mail_from("w1:p42", Some("kid"), "by name");
    let sent = deliver_later(&env, Duration::from_millis(100), msg);
    let out = ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    sent.await.unwrap();
    assert_eq!(out["reply"]["body"], "by name");
}

#[tokio::test]
async fn a_long_multi_line_json_reply_round_trips_byte_exact() {
    let env = mail_env(Opts::default()).await;
    script_working_child(&env.h.fake);
    let long_path = format!(
        "crates/{}/src/deeply/nested/module/file.rs",
        "segment".repeat(30)
    );
    let value = json!({
        "findings": (0..12).map(|n| json!({
            "path": long_path, "line": n, "note": format!("quote \" backslash \\ tab\t é ✓ {n}"),
        })).collect::<Vec<_>>(),
        "summary": "line one\nline two\n",
    });
    let body = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    assert!(body.len() > 2000 && body.len() <= 16 * 1024);
    let sent = deliver_later(
        &env,
        Duration::from_millis(100),
        mail_from("w1:p9", Some("kid"), &body),
    );
    let out = ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    sent.await.unwrap();
    assert_eq!(out["reply"]["body"].as_str().unwrap(), body);
}

#[tokio::test]
async fn the_instruction_ends_with_the_parent_pane_id_when_the_caller_has_no_name() {
    let env = mail_env(Opts::default()).await;
    script_working_child(&env.h.fake);
    let sent = deliver_later(
        &env,
        Duration::from_millis(50),
        mail_from("w1:p9", Some("kid"), "x"),
    );
    ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    sent.await.unwrap();
    let prompt = &requests(&env.h.fake, "agent.prompt")[0]["params"];
    assert_eq!(
        prompt["text"].as_str().unwrap(),
        format!("go\n\n{}", paragraph("w1:p1"))
    );
}

fn snapshot_with_agents(with_new: bool, extra: Vec<Value>) -> Value {
    let mut snapshot = snapshot(with_new, None);
    snapshot["snapshot"]["agents"]
        .as_array_mut()
        .unwrap()
        .extend(extra);
    snapshot
}

fn script_named_caller(fake: &FakeHerdr) {
    let extra = || vec![harness::agent("w1:p1", "w1:t1", "w1", "boss")];
    fake.reply_seq(
        "session.snapshot",
        vec![
            snapshot_with_agents(false, extra()),
            snapshot_with_agents(true, extra()),
        ],
    );
}

#[tokio::test]
async fn the_instruction_names_the_parent_when_the_caller_is_a_named_agent() {
    let env = mail_env(Opts::default()).await;
    script_working_child(&env.h.fake);
    script_named_caller(&env.h.fake);
    // The caller's name inbox is where the child's mail goes.
    let msg = mail_from("w1:p9", Some("kid"), "named parent");
    let mailbox = env.mailbox();
    let sent = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        mailbox
            .deliver(&herdr_mcp::mailbox::name_key("boss"), &msg)
            .unwrap();
    });
    let out = ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    sent.await.unwrap();
    assert_eq!(out["reply"]["body"], "named parent");
    let prompt = &requests(&env.h.fake, "agent.prompt")[0]["params"];
    assert_eq!(
        prompt["text"].as_str().unwrap(),
        format!("go\n\n{}", paragraph("boss"))
    );
}

#[tokio::test]
async fn a_child_that_settles_without_mailing_gets_the_grace_then_null_and_the_excerpt() {
    let env = mail_env(Opts::default()).await;
    script_launch(
        &env.h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let began = Instant::now();
    let out = ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;

    assert!(
        began.elapsed() >= Duration::from_millis(300),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(out["reply"], Value::Null);
    assert_eq!(
        out["note"],
        "child finished without mailing a result; read its pane"
    );
    assert_eq!(out["status"], "done");
    assert_eq!(out["excerpt"], "all done");
    assert_eq!(out["timed_out"], false);
}

#[tokio::test]
async fn mail_that_arrives_during_the_grace_is_still_the_reply() {
    let env = mail_env(Opts::default()).await;
    script_launch(
        &env.h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let sent = deliver_later(
        &env,
        Duration::from_millis(150),
        mail_from("w1:p9", Some("kid"), "just in time"),
    );
    let out = ok(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    sent.await.unwrap();
    assert_eq!(out["reply"]["body"], "just in time");
    assert_eq!(out["status"], "done");
    assert!(out.get("excerpt").is_none(), "{out}");
    assert!(out.get("note").is_none(), "{out}");
}

#[tokio::test]
async fn a_timeout_returns_a_mail_rewait_for_the_child() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let inner = capped_inner(&fake, &log_dir, 900);
    script_working_child(&fake);

    let out = launch::run(
        &inner,
        None,
        CancellationToken::new(),
        None,
        launch_params(json!({
            "kind": "codex", "instructions": "go", "wait": "turn", "reply": "mail",
        })),
    )
    .await
    .unwrap();

    assert_eq!(out["timed_out"], true, "{out}");
    assert_eq!(out["reply"], Value::Null);
    assert_eq!(out["rewait"], json!({"mail": {"from": "kid"}}));
}

#[tokio::test]
async fn an_async_mail_launch_returns_pending_and_the_idle_parent_is_nudged_by_child_mail() {
    let env = mail_env(Opts::default()).await;
    // Once the prompt is sent, agent.get answers for the parent: idle, so it can be nudged.
    let mut idle_parent = harness::agent("w1:p1", "w1:t1", "w1", "boss");
    idle_parent["agent_status"] = json!("idle");
    script_launch_then(
        &env.h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
        json!({"type": "agent_info", "agent": idle_parent}),
    );

    let out = ok(
        &env.h,
        "herdr_launch",
        mail_launch_args(json!({"wait": "none"})),
    )
    .await;

    assert_eq!(out["reply"], "pending");
    assert_eq!(
        out["note"],
        "The agent will mail its result to you; end your turn, and the mail nudge will wake you."
    );
    assert_eq!(out["phase"], "prompted");
    assert_eq!(count(&env.h.fake, "events.subscribe"), 0);

    // The child mails the parent later. The parent is an idle agent by then.
    let agents = vec![
        harness::agent("w1:p1", "w1:t1", "w1", "boss"),
        harness::agent("w1:p9", "w1:t9", "w1", "kid"),
    ];
    env.h
        .fake
        .reply_seq("session.snapshot", vec![snapshot_with_agents(true, agents)]);
    env.h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p9"}),
        process_info("w1:p9", 9500, 9500),
    );
    env.h.fake.reply("agent.prompt", prompted_kid("working", 8));
    let child = child_bridge(&env).await;
    let sent = call(
        &child,
        "herdr_mail_send",
        json!({"to": "boss", "body": "all finished"}),
    )
    .await;
    assert_ne!(sent.is_error, Some(true), "{sent:?}");
    let sent = sent.structured_content.unwrap();
    assert_eq!(sent["nudged"], true, "{sent}");
    let prompts = requests(&env.h.fake, "agent.prompt");
    let last = &prompts.last().unwrap()["params"];
    assert_eq!(last["target"], "w1:p1");
    assert!(
        last["text"]
            .as_str()
            .unwrap()
            .starts_with("New herdr mail from kid."),
        "{last}"
    );
}

/// A second bridge on the same fake and mailbox, running as the child in w1:p9.
async fn child_bridge(env: &MailEnv) -> Harness {
    let config = BridgeConfig {
        socket_path: env.h.fake.path().to_path_buf(),
        profile: Profile::new(None, Some(Duration::from_secs(5))),
        mode: ToolMode::Read,
        ancestry: Arc::new(StaticAncestry(vec![9500, 1])),
        caller_env: CallerEnv {
            pane_id: Some("w1:p9".into()),
            tab_id: Some("w1:t9".into()),
            workspace_id: Some("w1".into()),
        },
        logger: Logger::to_dir(env.state.path().join("child-logs")).unwrap(),
        install_env: None,
    };
    let dir = env.state.path().to_path_buf();
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let running = HerdrMcp::new(config)
            .with_mailbox_state_dir(dir)
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
        fake: FakeHerdr::start().await,
        client,
        _log_dir: tempfile::tempdir().unwrap(),
    }
}

fn assert_no_creation(fake: &FakeHerdr) {
    for method in [
        "tab.create",
        "pane.split",
        "workspace.create",
        "agent.start",
        "agent.prompt",
        "pane.close",
    ] {
        assert_eq!(count(fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn reply_mail_from_an_unverified_caller_is_refused_before_anything_is_created() {
    let env = mail_env(Opts::default()).await;
    // No pane.process_info script for the caller: its shell cannot be matched.
    env.h
        .fake
        .reply_seq("session.snapshot", vec![snapshot(false, None)]);
    let code = err_code(&env.h, "herdr_launch", mail_launch_args(json!({}))).await;
    assert_eq!(code, "invalid_params");
    assert_no_creation(&env.h.fake);
}

#[tokio::test]
async fn reply_mail_with_an_unsupported_kind_is_refused_before_anything_is_created() {
    let sandbox = Sandbox::new();
    let env = mail_env(Opts {
        install_env: Some(sandbox.env.clone()),
        ..Opts::default()
    })
    .await;
    caller_ok(&env.h.fake);
    env.h
        .fake
        .reply_seq("session.snapshot", vec![snapshot(false, None)]);
    let result = err(
        &env.h,
        "herdr_launch",
        mail_launch_args(json!({"kind": "opencode"})),
    )
    .await;
    assert_eq!(result["code"], "reply_unsupported");
    assert_no_creation(&env.h.fake);
}

#[tokio::test]
async fn reply_mail_needs_instructions() {
    let env = mail_env(Opts::default()).await;
    let code = err_code(
        &env.h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "reply": "mail"}),
    )
    .await;
    assert_eq!(code, "invalid_params");
    assert!(env.h.fake.requests().is_empty());
}

#[tokio::test]
async fn reply_screen_is_the_default_and_adds_nothing() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .await;
    assert!(out.get("reply").is_none(), "{out}");
    assert_eq!(out["excerpt"], "all done");
    let prompt = &requests(&h.fake, "agent.prompt")[0]["params"];
    assert_eq!(prompt["text"], "go");
}

#[tokio::test]
async fn a_long_unwrapped_line_survives_in_the_excerpt() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    let long = format!(
        "path: /srv/{}/config.json done",
        "very-long-directory-name/".repeat(20)
    );
    assert!(long.len() > 400);
    h.fake
        .reply_seq("pane.read", vec![read_result(&format!("first\n{long}\n"))]);
    let out = ok(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .await;
    assert!(
        out["excerpt"].as_str().unwrap().lines().any(|l| l == long),
        "{out}"
    );
    for read in requests(&h.fake, "pane.read") {
        assert_eq!(read["params"]["source"], "recent_unwrapped");
    }
}

fn assert_no_phase(out: &Value) {
    assert!(out.get("phase").is_none(), "{out}");
}

#[tokio::test]
async fn a_failed_agent_tag_after_start_is_provenance_failed_and_closes_the_tab() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.on_seq(
        "pane.report_metadata",
        vec![
            Script::Reply(ok_reply()),
            Script::error("invalid_metadata_token", "bad"),
        ],
    );
    h.fake.reply("tab.close", ok_reply());

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "provenance_failed");
    assert_eq!(out["phase"], "started");
    assert_eq!(out["pane"], "w1:p9");
    assert_eq!(count(&h.fake, "agent.start"), 1);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
    assert_eq!(requests(&h.fake, "tab.close").len(), 1);
}

#[tokio::test]
async fn an_agent_get_error_while_waiting_for_ready_closes_the_tab_without_details() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake
        .on_seq("agent.get", vec![Script::error("pane_not_found", "gone")]);
    h.fake.reply("tab.close", ok_reply());

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "pane_not_found");
    assert_no_phase(&out);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
    let closed = requests(&h.fake, "tab.close");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["params"]["tab_id"], "w1:t9");
}

#[tokio::test]
async fn an_agent_that_exits_in_a_created_tab_reports_cleaned_up_true() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![kid("unknown", 1, false, true)],
        },
    );
    h.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    h.fake.reply("tab.close", ok_reply());

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go", "start_timeout_ms": 20000}),
    )
    .await;

    assert_eq!(out["code"], "agent_exited");
    assert_eq!(out["phase"], "started");
    assert_eq!(out["cleaned_up"], true);
    assert_eq!(count(&h.fake, "tab.close"), 1);
}

#[tokio::test]
async fn an_agent_that_exits_in_an_existing_pane_reports_cleaned_up_false_and_closes_nothing() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: vec![kid("unknown", 1, false, true)],
        },
    );
    h.fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p2"}),
        process_info("w1:p2", 200, 200),
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go",
               "where": {"pane": "w1:p2"}, "start_timeout_ms": 20000}),
    )
    .await;

    assert_eq!(out["code"], "agent_exited");
    assert_eq!(out["phase"], "started");
    assert_eq!(out["pane"], "w1:p2");
    assert_eq!(out["cleaned_up"], false);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn a_socket_error_from_send_keys_during_dialogs_has_no_details_and_keeps_the_pane() {
    let h = start(Opts::default()).await;
    launch_with_dialog(&h, &[HOOK_DIALOG]);
    h.fake.on_seq(
        "agent.send_keys",
        vec![Script::error("pane_not_found", "gone")],
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "codex", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "pane_not_found");
    assert_no_phase(&out);
    assert_eq!(count(&h.fake, "agent.send_keys"), 1);
    assert_eq!(count(&h.fake, "agent.prompt"), 0);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn a_prompt_refused_while_blocked_is_agent_blocked_with_phase_started_and_keeps_the_pane() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.on_seq(
        "agent.prompt",
        vec![Script::error("agent_blocked", "agent is blocked")],
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "agent_blocked");
    assert_eq!(out["phase"], "started");
    assert_eq!(out["pane"], "w1:p9");
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn any_other_prompt_error_passes_through_without_details_and_keeps_the_pane() {
    let h = start(Opts::default()).await;
    script_launch(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
    );
    h.fake.on_seq(
        "agent.prompt",
        vec![Script::error("pane_not_found", "gone")],
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go"}),
    )
    .await;

    assert_eq!(out["code"], "pane_not_found");
    assert_no_phase(&out);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn a_wait_error_reports_phase_waiting_with_the_turn_and_keeps_the_pane() {
    let h = start(Opts::default()).await;
    script_launch_with(
        &h.fake,
        Launch {
            busy: 0,
            ready: ready_idle(),
        },
        Script::error("pane_not_found", "gone"),
    );

    let out = err(
        &h,
        "herdr_launch",
        json!({"kind": "claude", "name": "kid", "instructions": "go", "wait": "turn"}),
    )
    .await;

    assert_eq!(out["code"], "agent_not_running");
    assert_eq!(out["phase"], "waiting");
    assert_eq!(out["pane"], "w1:p9");
    assert_eq!(out["turn"]["after"], 7);
    assert_no_cleanup(&h.fake);
}

#[tokio::test]
async fn cancel_while_the_start_is_retrying_keeps_the_pane_and_reports_phase_placed() {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let mut inner = direct_inner(&fake, ToolMode::Control, &log_dir);
    inner.launch.start_retry_window = Duration::from_secs(30);
    caller_ok(&fake);
    fake.reply_seq(
        "session.snapshot",
        vec![snapshot(false, None), snapshot(true, None)],
    );
    fake.reply("tab.create", tab_created());
    fake.reply("pane.report_metadata", ok_reply());
    fake.on_seq(
        "agent.start",
        vec![Script::error("agent_pane_busy", "busy")],
    );
    fake.reply_seq("pane.process_info", vec![process_info("w1:p9", 200, 200)]);
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        canceller.cancel();
    });

    let result = launch::run(&inner, None, cancel, None, launch_params(json!({}))).await;
    task.await.unwrap();

    let err = result.unwrap_err();
    assert_eq!(err.code(), "cancelled");
    let details = err.details().expect("details");
    assert_eq!(details["phase"], "placed");
    assert_eq!(details["pane"], "w1:p9");
    assert_no_cleanup(&fake);
}

// ---- herdr_send reply: "mail" ---------------------------------------------------------------

fn prompt_text(fake: &FakeHerdr) -> String {
    fake.requests()
        .into_iter()
        .find(|r| r["method"] == "agent.prompt")
        .map(|r| r["params"]["text"].as_str().unwrap().to_owned())
        .unwrap()
}

#[tokio::test]
async fn send_reply_mail_with_wait_turn_returns_the_mailed_answer() {
    let env = mail_env(Opts::default()).await;
    script_send(
        &env.h.fake,
        Some("w1:p1"),
        vec![rev("idle", 3), rev("working", 5)],
    );
    let mut answer = mail_from("w1:p4", Some("rev"), "the whole answer");
    answer.ts_ms = now_ms() + 100;
    let sent = deliver_later(&env, Duration::from_millis(200), answer.clone());

    let out = ok(
        &env.h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "wait": "turn", "reply": "mail"}),
    )
    .await;
    sent.await.unwrap();

    assert_eq!(out["delivery"], "started");
    assert_eq!(out["reply"]["body"], "the whole answer", "{out}");
    assert_eq!(out["reply"]["id"], answer.id);
    assert_eq!(out["pane"], "w1:p4");
    assert_eq!(
        prompt_text(&env.h.fake),
        format!("go\n\n{}", paragraph("w1:p1"))
    );
    assert_eq!(env.unread(), 0);
}

#[tokio::test]
async fn send_reply_mail_without_a_wait_returns_pending() {
    let env = mail_env(Opts::default()).await;
    script_send(&env.h.fake, Some("w1:p1"), vec![rev("idle", 3)]);

    let out = ok(
        &env.h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "reply": "mail"}),
    )
    .await;

    assert_eq!(out["reply"], "pending");
    assert!(
        out["note"].as_str().unwrap().contains("mail nudge"),
        "{out}"
    );
}

#[tokio::test]
async fn send_reply_mail_to_a_queued_prompt_is_pending_since_there_is_no_turn() {
    let env = mail_env(Opts::default()).await;
    script_send(
        &env.h.fake,
        Some("w1:p1"),
        vec![rev_kind("working", "claude")],
    );
    screens(&env.h.fake, &[QUEUED_SCREEN]);

    let out = ok(
        &env.h,
        "herdr_send",
        json!({"target": "rev", "prompt": "next", "wait": "turn", "reply": "mail"}),
    )
    .await;

    assert_eq!(out["delivery"], "queued");
    assert_eq!(out["reply"], "pending");
}

#[tokio::test]
async fn send_reply_mail_from_an_unverified_caller_types_nothing() {
    let env = mail_env(Opts::default()).await;
    // No pane.process_info script for the caller: its shell cannot be matched.
    env.h
        .fake
        .reply_seq("session.snapshot", vec![snapshot(false, None)]);
    env.h.fake.reply_seq("agent.get", vec![rev("idle", 3)]);

    let code = err_code(
        &env.h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "reply": "mail"}),
    )
    .await;

    assert_eq!(code, "invalid_params");
    assert_nothing_typed(&env.h.fake);
}

#[tokio::test]
async fn send_reply_applies_to_a_prompt_only() {
    let h = start(Opts::default()).await;
    let code = err_code(
        &h,
        "herdr_send",
        json!({"target": "rev", "keys": ["Enter"], "reply": "mail"}),
    )
    .await;
    assert_eq!(code, "invalid_params");
}

#[tokio::test]
async fn send_reply_mail_ignores_mail_the_agent_sent_before_the_prompt() {
    let env = mail_env(Opts::default()).await;
    script_send(
        &env.h.fake,
        Some("w1:p1"),
        vec![rev("idle", 3), rev("working", 5)],
    );
    let stale = mail_from("w1:p4", Some("rev"), "the answer to the last prompt");
    env.mailbox()
        .deliver(&recipient_key("w1:p1"), &stale)
        .unwrap();
    let mut fresh = mail_from("w1:p4", Some("rev"), "the answer to this prompt");
    fresh.ts_ms = now_ms() + 150;
    let sent = deliver_later(&env, Duration::from_millis(300), fresh);

    let out = ok(
        &env.h,
        "herdr_send",
        json!({"target": "rev", "prompt": "go", "wait": "turn", "reply": "mail"}),
    )
    .await;
    sent.await.unwrap();

    assert_eq!(out["reply"]["body"], "the answer to this prompt", "{out}");
    assert_eq!(env.unread(), 1, "the earlier mail stays unread");
}
