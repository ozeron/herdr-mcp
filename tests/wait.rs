#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_herdr::{FakeHerdr, Script, StreamEnd};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::herdr::{AgentStatus, Client, HerdrError};
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use herdr_mcp::turn::{Turn, TurnFrom};
use herdr_mcp::wait::{self, Condition, Request, Tuning};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig,
    ClientRequest, Implementation, ProgressNotificationParam,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tempfile::TempDir;

const PANE: &str = "w1:p4";
const TERMINAL: &str = "t-w1:p4";

fn fast() -> Tuning {
    Tuning {
        poll_interval: Duration::from_millis(20),
        progress_interval: Duration::from_secs(60),
        observe_window: Duration::from_millis(150),
        recheck_delay: Duration::from_millis(30),
        pane_interval: Duration::from_millis(10),
    }
}

fn agent_at(pane: &str, terminal: &str, status: &str, seq: u64, completion: Option<u64>) -> Value {
    json!({
        "terminal_id": terminal, "name": "rev", "agent": "codex", "agent_status": status,
        "pane_id": pane, "tab_id": "w1:t2", "workspace_id": "w1", "focused": false,
        "revision": 1, "state_change_seq": seq, "completion_seq": completion,
        "interactive_ready": true, "launch_pending": false,
    })
}

fn info(status: &str, seq: u64, completion: Option<u64>) -> Value {
    json!({"type": "agent_info", "agent": agent_at(PANE, TERMINAL, status, seq, completion)})
}

fn status_event(status: &str) -> Value {
    json!({"event": "pane.agent_status_changed",
           "data": {"agent": "codex", "agent_status": status, "pane_id": PANE, "workspace_id": "w1"}})
}

fn token(after: u64, from: TurnFrom) -> Turn {
    Turn {
        pane: PANE.into(),
        terminal_id: TERMINAL.into(),
        shell_pid: None,
        after,
        from,
    }
}

fn process_info(pane_id: &str, shell: u32, fg: u32) -> Value {
    json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": shell, "foreground_process_group_id": fg,
        "tty": "/dev/ttys001", "foreground_processes": []}})
}

fn read_result(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:t2", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 7, "truncated": false}})
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

fn hold_stream(fake: &FakeHerdr, events: Vec<Value>) {
    fake.on(
        "events.subscribe",
        Script::Stream {
            events,
            end: StreamEnd::Hold,
        },
    );
}

fn always(fake: &FakeHerdr, method: &str, result: Value) {
    fake.reply_seq(method, vec![result]);
}

fn client(fake: &FakeHerdr) -> Client {
    Client::new(fake.path()).unwrap()
}

fn request(condition: Condition, cap_ms: u64) -> Request {
    Request {
        pane: PANE.into(),
        condition,
        cap: Duration::from_millis(cap_ms),
    }
}

async fn run(fake: &FakeHerdr, tuning: &Tuning, request: Request) -> Result<Value, HerdrError> {
    wait::wait(&client(fake), tuning, request, None).await
}

async fn run_ok(fake: &FakeHerdr, tuning: &Tuning, request: Request) -> Value {
    run(fake, tuning, request).await.unwrap()
}

fn code(err: &HerdrError) -> &str {
    err.code()
}

fn assert_untouched(fake: &FakeHerdr) {
    for method in [
        "pane.send_input",
        "pane.send_text",
        "pane.send_keys",
        "agent.send_keys",
        "agent.prompt",
        "pane.close",
        "tab.close",
    ] {
        assert_eq!(count(fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn an_event_wakes_the_wait_and_agent_get_decides_completion() {
    let fake = FakeHerdr::start().await;
    hold_stream(&fake, vec![status_event("idle")]);
    fake.reply_seq(
        "agent.get",
        vec![info("working", 41, None), info("idle", 43, Some(43))],
    );
    let tuning = Tuning {
        poll_interval: Duration::from_secs(30),
        ..fast()
    };

    let started = Instant::now();
    let out = run_ok(
        &fake,
        &tuning,
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "waited on the poll"
    );
    assert_eq!(out["matched"], true);
    assert_eq!(out["timed_out"], false);
    assert_eq!(out["status"], "idle");
    assert_eq!(out["superseded"], false);
    assert_eq!(out["state_change_seq"], 43);
    assert_eq!(out["completion_seq"], 43);
    let methods: Vec<String> = fake
        .requests()
        .iter()
        .map(|r| r["method"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(methods[0], "events.subscribe", "{methods:?}");
    assert_eq!(methods[1], "agent.get", "{methods:?}");
    let sub = &requests(&fake, "events.subscribe")[0]["params"]["subscriptions"];
    assert!(
        sub.as_array()
            .unwrap()
            .iter()
            .any(|s| s["type"] == "pane.agent_status_changed" && s["pane_id"] == PANE),
        "{sub}"
    );
    assert_untouched(&fake);
}

#[tokio::test]
async fn a_wait_started_after_completion_matches_at_once() {
    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("done", 43, Some(43)));

    let out = wait::wait_turn(
        &client(&fake),
        &fast(),
        token(41, TurnFrom::Working),
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "done");
    assert_eq!(count(&fake, "agent.get"), 1);
}

#[tokio::test]
async fn a_codex_turn_that_settles_to_unknown_matches_with_a_read_first_note() {
    let fake = FakeHerdr::start().await;
    fake.reply_seq(
        "agent.get",
        vec![info("working", 41, None), info("unknown", 43, None)],
    );

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "unknown");
    assert!(out["note"].as_str().unwrap().contains("read"), "{out}");
}

#[tokio::test]
async fn a_missed_settle_reports_superseded() {
    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("working", 45, None));

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "working");
    assert_eq!(out["superseded"], true);
}

#[tokio::test]
async fn blocked_matches_and_returns_a_token_that_rebases_when_approved() {
    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("blocked", 42, None));
    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;
    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "blocked");
    assert_eq!(
        out["turn"],
        json!({"pane": PANE, "terminal_id": TERMINAL, "shell_pid": null, "after": 42, "from": "blocked"})
    );

    let fake = FakeHerdr::start().await;
    fake.reply_seq(
        "agent.get",
        vec![
            info("blocked", 42, None),
            info("working", 43, None),
            info("idle", 44, Some(44)),
        ],
    );
    let resumed: Turn = serde_json::from_value(out["turn"].clone()).unwrap();
    let out = run_ok(&fake, &fast(), request(Condition::Turn(resumed), 5000)).await;
    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "idle");
    assert_eq!(out["superseded"], false);
    assert_eq!(count(&fake, "agent.get"), 3);
}

#[tokio::test]
async fn a_rebased_wait_that_times_out_returns_the_rebased_token() {
    let fake = FakeHerdr::start().await;
    fake.reply_seq(
        "agent.get",
        vec![info("blocked", 42, None), info("working", 43, None)],
    );

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(42, TurnFrom::Blocked)), 150),
    )
    .await;

    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], true);
    assert_eq!(out["turn"]["after"], 43);
    assert_eq!(out["turn"]["from"], "working");
}

#[tokio::test]
async fn a_new_terminal_id_under_the_same_pane_is_stale() {
    let fake = FakeHerdr::start().await;
    fake.reply(
        "agent.get",
        json!({"type": "agent_info", "agent": agent_at(PANE, "t-new", "idle", 2, None)}),
    );

    let err = run(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await
    .unwrap_err();

    assert_eq!(code(&err), "turn_stale");
}

#[tokio::test]
async fn a_changed_shell_pid_is_stale() {
    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("done", 43, Some(43)));
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": PANE}),
        process_info(PANE, 999, 999),
    );
    let turn = Turn {
        shell_pid: Some(4242),
        ..token(41, TurnFrom::Working)
    };

    let err = run(&fake, &fast(), request(Condition::Turn(turn), 5000))
        .await
        .unwrap_err();

    assert_eq!(code(&err), "turn_stale");
}

#[tokio::test]
async fn a_pane_that_lost_its_agent_but_has_a_new_terminal_is_stale() {
    let fake = FakeHerdr::start().await;
    fake.on("agent.get", Script::error("agent_not_running", "no agent"));
    fake.reply("agent.list", json!({"type": "agent_list", "agents": []}));
    fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": {
            "version": "0.9.3", "protocol": 22, "layouts": [], "workspaces": [], "tabs": [],
            "agents": [],
            "panes": [{"pane_id": PANE, "terminal_id": "t-new", "tab_id": "w1:t2",
                       "workspace_id": "w1", "focused": false, "agent_status": "unknown",
                       "revision": 1}]}}),
    );

    let err = run(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await
    .unwrap_err();

    assert_eq!(code(&err), "turn_stale");
}

#[tokio::test]
async fn events_lost_resubscribes_and_resyncs() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Lost,
        },
    );
    hold_stream(&fake, vec![]);
    fake.reply_seq(
        "agent.get",
        vec![info("working", 41, None), info("done", 43, Some(43))],
    );
    let tuning = Tuning {
        poll_interval: Duration::from_secs(30),
        ..fast()
    };

    let started = Instant::now();
    let out = run_ok(
        &fake,
        &tuning,
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "waited on the poll"
    );
    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "done");
    assert_eq!(count(&fake, "events.subscribe"), 2);
}

#[tokio::test]
async fn a_stream_that_closes_falls_back_to_polling() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "events.subscribe",
        Script::Stream {
            events: vec![],
            end: StreamEnd::Close,
        },
    );
    fake.reply_seq(
        "agent.get",
        vec![info("working", 41, None), info("done", 43, Some(43))],
    );

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
}

#[tokio::test]
async fn the_cap_reports_timed_out_and_never_touches_the_agent() {
    let fake = FakeHerdr::start().await;
    hold_stream(&fake, vec![]);
    always(&fake, "agent.get", info("working", 41, None));

    let started = Instant::now();
    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 200),
    )
    .await;

    assert!(started.elapsed() >= Duration::from_millis(190));
    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], true);
    assert_eq!(out["status"], "working");
    assert_eq!(out["turn"]["after"], 41);
    assert_untouched(&fake);
}

#[tokio::test]
async fn a_moved_pane_is_found_again_by_terminal_id() {
    let fake = FakeHerdr::start().await;
    fake.on_params(
        "agent.get",
        json!({"target": PANE}),
        Script::error("pane_not_found", "moved"),
    );
    fake.reply_params(
        "agent.get",
        json!({"target": "w2:p1"}),
        json!({"type": "agent_info", "agent": agent_at("w2:p1", TERMINAL, "done", 43, Some(43))}),
    );
    always(
        &fake,
        "agent.list",
        json!({"type": "agent_list", "agents": [agent_at("w2:p1", TERMINAL, "working", 41, None)]}),
    );

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "done");
    assert_eq!(out["pane"], "w2:p1");
}

#[tokio::test]
async fn a_closed_pane_is_agent_not_running() {
    let fake = FakeHerdr::start().await;
    fake.on_seq(
        "agent.get",
        vec![
            Script::Reply(info("working", 41, None)),
            Script::error("pane_not_found", "closed"),
        ],
    );
    fake.reply("agent.list", json!({"type": "agent_list", "agents": []}));
    fake.reply(
        "session.snapshot",
        json!({"type": "session_snapshot", "snapshot": {
            "version": "0.9.3", "protocol": 22, "layouts": [], "workspaces": [], "tabs": [],
            "panes": [], "agents": []}}),
    );

    let err = run(
        &fake,
        &fast(),
        request(Condition::Turn(token(41, TurnFrom::Working)), 5000),
    )
    .await
    .unwrap_err();

    assert_eq!(code(&err), "agent_not_running");
}

#[tokio::test]
async fn until_matches_a_listed_status_and_reports_it() {
    let fake = FakeHerdr::start().await;
    fake.reply_seq(
        "agent.get",
        vec![
            info("working", 41, None),
            info("blocked", 42, None),
            info("idle", 44, Some(44)),
        ],
    );

    let out = run_ok(
        &fake,
        &fast(),
        request(
            Condition::Until(vec![AgentStatus::Idle, AgentStatus::Done]),
            5000,
        ),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "idle");
    assert_eq!(count(&fake, "agent.get"), 3);
}

#[tokio::test]
async fn until_matches_at_once_when_already_in_the_state_and_times_out_otherwise() {
    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("blocked", 42, None));
    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Until(vec![AgentStatus::Blocked]), 5000),
    )
    .await;
    assert_eq!(out["matched"], true);
    assert_eq!(count(&fake, "agent.get"), 1);

    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("working", 41, None));
    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::Until(vec![AgentStatus::Idle]), 150),
    )
    .await;
    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], true);
    assert_eq!(out["status"], "working");
    assert_untouched(&fake);
}

#[tokio::test]
async fn until_on_a_closed_pane_is_agent_not_running() {
    let fake = FakeHerdr::start().await;
    fake.on("agent.get", Script::error("pane_not_found", "closed"));

    let err = run(
        &fake,
        &fast(),
        request(Condition::Until(vec![AgentStatus::Idle]), 5000),
    )
    .await
    .unwrap_err();

    assert_eq!(code(&err), "agent_not_running");
}

#[tokio::test]
async fn output_matches_only_text_added_after_the_baseline() {
    let fake = FakeHerdr::start().await;
    fake.reply_seq(
        "pane.read",
        vec![
            read_result("ready earlier\n$ "),
            read_result("ready earlier\n$ "),
            read_result("ready earlier\n$ \nserver ready now\n"),
        ],
    );

    let out = run_ok(
        &fake,
        &fast(),
        request(
            Condition::Output {
                needle: "ready".into(),
            },
            5000,
        ),
    )
    .await;

    assert_eq!(out["matched"], true);
    let excerpt = out["excerpt"].as_str().unwrap();
    assert!(excerpt.contains("server ready now"), "{excerpt}");
    for read in requests(&fake, "pane.read") {
        assert_eq!(read["params"]["source"], "recent_unwrapped");
        assert_eq!(read["params"]["lines"], 1000);
    }
    assert_untouched(&fake);
}

#[tokio::test]
async fn output_times_out_with_the_tail() {
    let fake = FakeHerdr::start().await;
    always(&fake, "pane.read", read_result("$ building\n"));

    let out = run_ok(
        &fake,
        &fast(),
        request(
            Condition::Output {
                needle: "ready".into(),
            },
            150,
        ),
    )
    .await;

    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], true);
    assert!(out["excerpt"].as_str().unwrap().contains("building"));
}

#[tokio::test]
async fn output_on_a_closed_pane_is_agent_not_running() {
    let fake = FakeHerdr::start().await;
    fake.on("pane.read", Script::error("pane_not_found", "closed"));

    let err = run(
        &fake,
        &fast(),
        request(Condition::Output { needle: "x".into() }, 5000),
    )
    .await
    .unwrap_err();

    assert_eq!(code(&err), "agent_not_running");
}

fn process_seq(fake: &FakeHerdr, shell: u32, fronts: &[u32]) {
    fake.reply_seq(
        "pane.process_info",
        fronts
            .iter()
            .map(|fg| process_info(PANE, shell, *fg))
            .collect(),
    );
}

#[tokio::test]
async fn process_exit_with_a_pgid_matches_when_the_shell_is_confirmed_in_front() {
    let fake = FakeHerdr::start().await;
    process_seq(&fake, 200, &[777, 777, 200]);

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::ProcessExit { pgid: Some(777) }, 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["pgid"], 777);
    assert_eq!(count(&fake, "pane.process_info"), 4);
    assert_untouched(&fake);
}

#[tokio::test]
async fn process_exit_does_not_match_between_the_jobs_of_a_and_b() {
    let fake = FakeHerdr::start().await;
    process_seq(&fake, 200, &[777, 200, 888, 888, 200, 200]);

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::ProcessExit { pgid: Some(777) }, 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(count(&fake, "pane.process_info"), 6);
}

#[tokio::test]
async fn process_exit_without_a_pgid_follows_the_first_non_shell_group() {
    let fake = FakeHerdr::start().await;
    process_seq(&fake, 200, &[200, 555, 555, 200, 200]);

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::ProcessExit { pgid: None }, 5000),
    )
    .await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["pgid"], 555);
}

#[tokio::test]
async fn process_exit_that_never_sees_a_non_shell_group_is_no_active_work() {
    let fake = FakeHerdr::start().await;
    process_seq(&fake, 200, &[200]);

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::ProcessExit { pgid: None }, 5000),
    )
    .await;

    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], false);
    assert_eq!(out["result"], "no_active_work");
}

#[tokio::test]
async fn process_exit_that_runs_past_the_cap_times_out() {
    let fake = FakeHerdr::start().await;
    process_seq(&fake, 200, &[777]);

    let out = run_ok(
        &fake,
        &fast(),
        request(Condition::ProcessExit { pgid: Some(777) }, 150),
    )
    .await;

    assert_eq!(out["matched"], false);
    assert_eq!(out["timed_out"], true);
}

#[tokio::test]
async fn process_exit_on_a_closed_pane_is_agent_not_running() {
    let fake = FakeHerdr::start().await;
    fake.on(
        "pane.process_info",
        Script::error("pane_not_found", "closed"),
    );

    let err = run(
        &fake,
        &fast(),
        request(Condition::ProcessExit { pgid: Some(1) }, 5000),
    )
    .await
    .unwrap_err();

    assert_eq!(code(&err), "agent_not_running");
}

#[tokio::test]
async fn progress_ticks_arrive_in_increasing_order() {
    let fake = FakeHerdr::start().await;
    always(&fake, "agent.get", info("working", 41, None));
    let tuning = Tuning {
        progress_interval: Duration::from_millis(100),
        ..fast()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let out = wait::wait(
        &client(&fake),
        &tuning,
        request(Condition::Turn(token(41, TurnFrom::Working)), 350),
        Some(tx),
    )
    .await
    .unwrap();

    assert_eq!(out["timed_out"], true);
    let mut ticks = Vec::new();
    while let Ok(tick) = rx.try_recv() {
        ticks.push(tick.elapsed);
    }
    assert_eq!(ticks.len(), 3, "{ticks:?}");
    assert!(ticks.windows(2).all(|pair| pair[0] < pair[1]), "{ticks:?}");
}

struct Harness {
    fake: FakeHerdr,
    client: RunningService<RoleClient, Collector>,
    progress: Arc<Mutex<Vec<f64>>>,
    _log_dir: TempDir,
}

struct Collector(Arc<Mutex<Vec<f64>>>);

impl ClientHandler for Collector {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("test-client", "1.0.0"),
        )
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.0.lock().unwrap().push(params.progress);
    }
}

struct Opts {
    env_pane: Option<&'static str>,
    max_wait: Option<Duration>,
    tuning: Tuning,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env_pane: Some("w1:p1"),
            max_wait: Some(Duration::from_millis(300)),
            tuning: fast(),
        }
    }
}

async fn start(opts: Opts) -> Harness {
    let fake = FakeHerdr::start().await;
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, opts.max_wait),
        mode: ToolMode::Read,
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
    let server = HerdrMcp::new(config).with_wait_tuning(opts.tuning);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let progress = Arc::new(Mutex::new(Vec::new()));
    let client = Collector(Arc::clone(&progress))
        .serve(client_io)
        .await
        .unwrap();
    Harness {
        fake,
        client,
        progress,
        _log_dir: log_dir,
    }
}

async fn call(h: &Harness, args: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new("herdr_wait");
    params.arguments = args.as_object().cloned();
    h.client.call_tool(params).await.unwrap()
}

async fn ok(h: &Harness, args: Value) -> Value {
    let result = call(h, args).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.expect("structured content")
}

async fn err_code(h: &Harness, args: Value) -> String {
    let result = call(h, args).await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    result.structured_content.unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string()
}

fn pane_json(id: &str, tab: &str) -> Value {
    json!({
        "pane_id": id, "terminal_id": format!("t-{id}"), "tab_id": tab, "workspace_id": "w1",
        "focused": false, "cwd": "/work", "agent_status": "unknown", "revision": 1,
    })
}

fn snapshot() -> Value {
    json!({"type": "session_snapshot", "snapshot": {
        "version": "0.9.3", "protocol": 22, "layouts": [],
        "workspaces": [{"workspace_id": "w1", "number": 1, "label": "w1", "focused": true,
                        "pane_count": 2, "tab_count": 2, "active_tab_id": "w1:t1",
                        "agent_status": "idle"}],
        "tabs": [
            {"tab_id": "w1:t1", "workspace_id": "w1", "number": 1, "label": "t1",
             "focused": true, "pane_count": 1, "agent_status": "idle"},
            {"tab_id": "w1:t2", "workspace_id": "w1", "number": 2, "label": "t2",
             "focused": false, "pane_count": 1, "agent_status": "idle"}],
        "panes": [pane_json("w1:p1", "w1:t1"), pane_json(PANE, "w1:t2")],
        "agents": [agent_at(PANE, TERMINAL, "idle", 3, None)]}})
}

fn script_verified_caller(fake: &FakeHerdr) {
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
}

fn script_unverified_caller(fake: &FakeHerdr) {
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 31337, 31337),
    );
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": PANE}),
        process_info(PANE, 31338, 31338),
    );
    always(fake, "session.snapshot", snapshot());
}

fn token_json(after: u64, from: &str) -> Value {
    json!({"pane": PANE, "terminal_id": TERMINAL, "shell_pid": null, "after": after, "from": from})
}

#[tokio::test]
async fn the_tool_takes_the_token_back_as_json_and_matches() {
    let h = start(Opts::default()).await;
    script_verified_caller(&h.fake);
    always(&h.fake, "agent.get", info("done", 43, Some(43)));

    let out = ok(&h, json!({"turn": token_json(41, "working")})).await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["status"], "done");
    assert_eq!(count(&h.fake, "pane.send_input"), 0);
}

#[tokio::test]
async fn the_tool_resolves_an_agent_name_for_until() {
    let h = start(Opts::default()).await;
    script_verified_caller(&h.fake);
    always(&h.fake, "session.snapshot", snapshot());
    always(&h.fake, "agent.get", info("idle", 3, None));

    let out = ok(&h, json!({"target": "rev", "until": ["idle", "done"]})).await;

    assert_eq!(out["matched"], true);
    assert_eq!(out["pane"], PANE);
}

#[tokio::test]
async fn the_tool_waits_for_output_and_process_exit() {
    let h = start(Opts::default()).await;
    script_verified_caller(&h.fake);
    always(&h.fake, "session.snapshot", snapshot());
    h.fake.reply_seq(
        "pane.read",
        vec![read_result("$ "), read_result("$ \nlistening on 3000\n")],
    );
    let out = ok(
        &h,
        json!({"target": PANE, "output": {"match": "listening on"}}),
    )
    .await;
    assert_eq!(out["matched"], true);

    process_seq(&h.fake, 200, &[777, 200, 200]);
    let out = ok(&h, json!({"target": PANE, "process_exit": {"pgid": 777}})).await;
    assert_eq!(out["matched"], true);
}

#[tokio::test]
async fn the_cap_comes_from_the_profile_and_is_noted() {
    let h = start(Opts::default()).await;
    script_verified_caller(&h.fake);
    always(&h.fake, "agent.get", info("working", 41, None));

    let started = Instant::now();
    let out = ok(
        &h,
        json!({"turn": token_json(41, "working"), "timeout_ms": 600000}),
    )
    .await;

    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(out["timed_out"], true);
    assert!(out["note"].as_str().unwrap().contains("300"), "{out}");
}

#[tokio::test]
async fn the_tool_refuses_the_callers_own_pane() {
    let h = start(Opts::default()).await;
    script_verified_caller(&h.fake);
    always(&h.fake, "session.snapshot", snapshot());

    let own = json!({"pane": "w1:p1", "terminal_id": "t-w1:p1", "shell_pid": null,
                     "after": 1, "from": "working"});
    assert_eq!(err_code(&h, json!({"turn": own})).await, "self_target");
    assert_eq!(
        err_code(&h, json!({"target": "w1:p1", "until": ["idle"]})).await,
        "self_target"
    );
    assert_eq!(
        err_code(&h, json!({"target": "w1:t1", "until": ["idle"]})).await,
        "self_target"
    );
    assert_eq!(count(&h.fake, "agent.get"), 0);
}

#[tokio::test]
async fn an_unverified_caller_is_guarded_on_the_env_named_pane() {
    let h = start(Opts::default()).await;
    script_unverified_caller(&h.fake);

    assert_eq!(
        err_code(&h, json!({"target": "w1:p1", "until": ["idle"]})).await,
        "self_target"
    );
    assert_eq!(count(&h.fake, "agent.get"), 0);
}

#[tokio::test]
async fn the_tool_rejects_ambiguous_or_empty_conditions() {
    let h = start(Opts::default()).await;
    script_verified_caller(&h.fake);
    always(&h.fake, "session.snapshot", snapshot());

    assert_eq!(
        err_code(&h, json!({"target": PANE})).await,
        "invalid_params"
    );
    assert_eq!(
        err_code(
            &h,
            json!({"target": PANE, "until": ["idle"], "output": {"match": "x"}})
        )
        .await,
        "invalid_params"
    );
    assert_eq!(
        err_code(&h, json!({"target": PANE, "until": []})).await,
        "invalid_params"
    );
    assert_eq!(
        err_code(&h, json!({"target": PANE, "output": {"match": ""}})).await,
        "invalid_params"
    );
    assert_eq!(
        err_code(&h, json!({"until": ["idle"]})).await,
        "invalid_params"
    );
}

#[tokio::test]
async fn progress_notifications_follow_the_wait() {
    let h = start(Opts {
        max_wait: Some(Duration::from_millis(350)),
        tuning: Tuning {
            progress_interval: Duration::from_millis(100),
            ..fast()
        },
        ..Opts::default()
    })
    .await;
    script_verified_caller(&h.fake);
    always(&h.fake, "agent.get", info("working", 41, None));

    let out = ok(&h, json!({"turn": token_json(41, "working")})).await;
    assert_eq!(out["timed_out"], true);

    let deadline = Instant::now() + Duration::from_secs(2);
    while h.progress.lock().unwrap().len() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = h.progress.lock().unwrap().clone();
    assert!(seen.len() >= 3, "{seen:?}");
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");
}

#[tokio::test]
async fn client_cancel_drops_the_connections_and_stops_requests() {
    let h = start(Opts {
        max_wait: Some(Duration::from_secs(30)),
        ..Opts::default()
    })
    .await;
    script_verified_caller(&h.fake);
    hold_stream(&h.fake, vec![]);
    always(&h.fake, "agent.get", info("working", 41, None));

    let mut params = CallToolRequestParams::new("herdr_wait");
    params.arguments = json!({"turn": token_json(41, "working")})
        .as_object()
        .cloned();
    let handle = h
        .client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while count(&h.fake, "agent.get") < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(count(&h.fake, "agent.get") >= 2, "wait never started");

    handle.cancel(Some("test".into())).await.unwrap();
    h.fake.wait_disconnect().await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let settled = h.fake.requests().len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        h.fake.requests().len(),
        settled,
        "requests continued after cancel"
    );
}
