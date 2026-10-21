#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use fake_herdr::FakeHerdr;
use harness::{count, process_info, requests, snapshot};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::mailbox::{recipient_key, session_key};
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use herdr_mcp::tools::mail::{WaitEnd, wait_for_mail};
use herdr_mcp::wait::Progress;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "MAIL-BODY-SENTINEL-7f3a9c";
const NUDGE_FROM_P1: &str = "New herdr mail from w1:p1. Read it with the herdr_mail_read tool; reply with herdr_mail_send if a reply is needed.";

type Client = rmcp::service::RunningService<RoleClient, ClientConfig>;

struct Env {
    fake: FakeHerdr,
    state: TempDir,
    logs: TempDir,
}

/// One bridge process. `pane` is the HERDR_PANE_ID it was started with; `shell` is the shell pid
/// that is its ancestor, or `None` for an external client no pane can be matched to.
async fn bridge(env: &Env, mode: ToolMode, pane: Option<&'static str>, shell: u32) -> Bridge {
    let config = BridgeConfig {
        socket_path: env.fake.path().to_path_buf(),
        profile: Profile::new(None, Some(Duration::from_secs(5))),
        mode,
        ancestry: Arc::new(StaticAncestry(vec![shell, 1])),
        caller_env: CallerEnv {
            pane_id: pane.map(Into::into),
            tab_id: pane.map(|_| "w1:t1".into()),
            workspace_id: pane.map(|_| "w1".into()),
        },
        logger: Logger::to_dir(env.logs.path().join("logs")).unwrap(),
        install_env: None,
    };
    let state = env.state.path().to_path_buf();
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
    Bridge { client }
}

struct Bridge {
    client: Client,
}

impl Bridge {
    async fn call(&self, name: &'static str, args: Value) -> rmcp::model::CallToolResult {
        let mut params = rmcp::model::CallToolRequestParams::new(name);
        params.arguments = args.as_object().cloned();
        self.client.call_tool(params).await.unwrap()
    }

    async fn ok(&self, name: &'static str, args: Value) -> Value {
        let result = self.call(name, args).await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        result.structured_content.expect("structured content")
    }

    async fn err_code(&self, name: &'static str, args: Value) -> String {
        let result = self.call(name, args).await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
        result.structured_content.unwrap()["code"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn agent_info(status: &str) -> Value {
    json!({"type": "agent_info", "agent": {
        "terminal_id": "t-w1:p4", "name": "rev", "agent": "codex", "agent_status": status,
        "pane_id": "w1:p4", "tab_id": "w1:t2", "workspace_id": "w1", "focused": false,
        "revision": 1, "state_change_seq": 3, "interactive_ready": true, "launch_pending": false}})
}

fn prompted() -> Value {
    let mut v = agent_info("working");
    v["type"] = json!("agent_prompted");
    v
}

async fn env() -> Env {
    let fake = FakeHerdr::start().await;
    fake.reply_seq("session.snapshot", vec![snapshot(None)]);
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p1"}),
        process_info("w1:p1", 4242, 4242),
    );
    fake.reply_params(
        "pane.process_info",
        json!({"pane_id": "w1:p4"}),
        process_info("w1:p4", 5555, 5555),
    );
    fake.on_params(
        "agent.get",
        json!({"target": "nobody"}),
        fake_herdr::Script::error("agent_not_found", "no agent"),
    );
    fake.reply_seq("agent.get", vec![agent_info("idle")]);
    fake.reply_seq("agent.prompt", vec![prompted()]);
    Env {
        fake,
        state: tempfile::tempdir().unwrap(),
        logs: tempfile::tempdir().unwrap(),
    }
}

/// Sender in w1:p1 and the agent "rev" in w1:p4, both control mode.
async fn pair(env: &Env) -> (Bridge, Bridge) {
    (
        bridge(env, ToolMode::Control, Some("w1:p1"), 4242).await,
        bridge(env, ToolMode::Control, Some("w1:p4"), 5555).await,
    )
}

fn inbox(env: &Env, pane: &str) -> PathBuf {
    env.state
        .path()
        .join("herdr-mcp/mailbox")
        .join(session_key(env.fake.path()))
        .join(recipient_key(pane))
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn send_then_read_round_trips_in_order() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    let a = sender
        .ok(
            "herdr_mail_send",
            json!({"to": "w1:p4", "body": "first", "notify": false}),
        )
        .await;
    assert_eq!(a["delivered"], true);
    assert_eq!(a["to"], json!({"pane": "w1:p4", "name": "rev"}));
    sender
        .ok(
            "herdr_mail_send",
            json!({"to": "w1:p4", "body": "second", "reply_to": a["id"], "notify": false}),
        )
        .await;

    let read = reader.ok("herdr_mail_read", json!({})).await;
    let msgs = read["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["body"], "first");
    assert_eq!(msgs[1]["body"], "second");
    assert_eq!(msgs[0]["id"], a["id"]);
    assert_eq!(msgs[0]["from"], json!({"pane": "w1:p1", "name": null}));
    assert_eq!(msgs[0]["to"], json!({"pane": "w1:p4", "name": "rev"}));
    assert_eq!(msgs[0]["reply_to"], Value::Null);
    assert_eq!(msgs[1]["reply_to"], a["id"]);
    assert!(msgs[0]["ts_ms"].as_u64().unwrap() > 0);
    assert_eq!(read["remaining"], 0);

    let again = reader.ok("herdr_mail_read", json!({})).await;
    assert_eq!(again["messages"], json!([]));
}

#[tokio::test]
async fn an_agent_name_resolves_to_its_pane_and_a_pane_id_records_the_name() {
    let env = env().await;
    let (sender, _reader) = pair(&env).await;
    let by_name = sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "hi", "notify": false}),
        )
        .await;
    assert_eq!(by_name["to"], json!({"pane": "w1:p4", "name": "rev"}));
    let plain = sender
        .ok(
            "herdr_mail_send",
            json!({"to": "w1:p2", "body": "hi", "notify": false}),
        )
        .await;
    assert_eq!(plain["to"], json!({"pane": "w1:p2", "name": null}));
    assert_eq!(files(&inbox(&env, "w1:p4").join("new")).len(), 1);
    assert_eq!(files(&inbox(&env, "w1:p2").join("new")).len(), 1);
    assert_eq!(
        sender
            .err_code("herdr_mail_send", json!({"to": "nobody", "body": "x"}))
            .await,
        "target_not_found"
    );
}

#[tokio::test]
async fn an_external_sender_has_no_pane_and_is_named_external() {
    let env = env().await;
    let external = bridge(&env, ToolMode::Read, None, 1).await;
    let reader = bridge(&env, ToolMode::Read, Some("w1:p4"), 5555).await;
    let sent = external
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "from outside"}),
        )
        .await;
    assert_eq!(sent["nudged"], true);
    let prompt = &requests(&env.fake, "agent.prompt")[0]["params"];
    assert_eq!(
        prompt["text"],
        "New herdr mail from external. Read it with the herdr_mail_read tool; reply with herdr_mail_send if a reply is needed."
    );
    let read = reader.ok("herdr_mail_read", json!({})).await;
    assert_eq!(
        read["messages"][0]["from"],
        json!({"pane": null, "name": "external"})
    );
}

#[tokio::test]
async fn an_unverified_caller_is_sent_as_external_even_with_a_pane_env() {
    let env = env().await;
    // HERDR_PANE_ID says w1:p1, but no ancestor matches its shell.
    let liar = bridge(&env, ToolMode::Read, Some("w1:p1"), 777).await;
    let reader = bridge(&env, ToolMode::Read, Some("w1:p4"), 5555).await;
    liar.ok(
        "herdr_mail_send",
        json!({"to": "rev", "body": "x", "notify": false}),
    )
    .await;
    let read = reader.ok("herdr_mail_read", json!({})).await;
    assert_eq!(
        read["messages"][0]["from"],
        json!({"pane": null, "name": "external"})
    );
}

#[tokio::test]
async fn sending_to_yourself_is_refused_unless_allowed() {
    let env = env().await;
    let (sender, _) = pair(&env).await;
    assert_eq!(
        sender
            .err_code("herdr_mail_send", json!({"to": "w1:p1", "body": "me"}))
            .await,
        "self_target"
    );
    assert!(files(&inbox(&env, "w1:p1").join("new")).is_empty());
    let out = sender
        .ok(
            "herdr_mail_send",
            json!({"to": "w1:p1", "body": "me", "allow_self": true}),
        )
        .await;
    assert_eq!(out["delivered"], true);
    let read = sender.ok("herdr_mail_read", json!({})).await;
    assert_eq!(read["messages"][0]["body"], "me");
}

#[tokio::test]
async fn an_idle_agent_gets_the_fixed_nudge_without_the_body() {
    let env = env().await;
    let (sender, _) = pair(&env).await;
    let out = sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": SENTINEL}))
        .await;
    assert_eq!(out["nudged"], true);
    assert!(out.get("nudge_skipped").is_none(), "{out}");
    let prompts = requests(&env.fake, "agent.prompt");
    assert_eq!(prompts.len(), 1);
    let params = &prompts[0]["params"];
    assert_eq!(params["target"], "w1:p4");
    assert_eq!(params["text"], NUDGE_FROM_P1);
    assert!(params.get("wait").is_none(), "{params}");
    assert!(!params.to_string().contains(SENTINEL));
}

#[tokio::test]
async fn a_done_agent_is_nudged_too() {
    let env = env().await;
    env.fake.reply_seq("agent.get", vec![agent_info("done")]);
    let (sender, _) = pair(&env).await;
    let out = sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": "x"}))
        .await;
    assert_eq!(out["nudged"], true);
}

#[tokio::test]
async fn a_working_or_blocked_agent_is_not_nudged() {
    for status in ["working", "blocked"] {
        let env = env().await;
        env.fake.reply_seq("agent.get", vec![agent_info(status)]);
        let (sender, _) = pair(&env).await;
        let out = sender
            .ok("herdr_mail_send", json!({"to": "rev", "body": "x"}))
            .await;
        assert_eq!(out["delivered"], true, "{status}");
        assert_eq!(out["nudged"], false, "{status}");
        assert!(
            out["nudge_skipped"].as_str().unwrap().contains(status),
            "{out}"
        );
        assert_eq!(count(&env.fake, "agent.prompt"), 0, "{status}");
    }
}

#[tokio::test]
async fn a_plain_pane_is_not_nudged() {
    let env = env().await;
    let (sender, _) = pair(&env).await;
    let out = sender
        .ok("herdr_mail_send", json!({"to": "w1:p2", "body": "x"}))
        .await;
    assert_eq!(out["delivered"], true);
    assert_eq!(out["nudged"], false);
    assert_eq!(out["nudge_skipped"], "not_an_agent");
    assert_eq!(count(&env.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn notify_false_skips_the_nudge() {
    let env = env().await;
    let (sender, _) = pair(&env).await;
    let out = sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "x", "notify": false}),
        )
        .await;
    assert_eq!(out["nudged"], false);
    assert_eq!(out["nudge_skipped"], "notify_false");
    assert_eq!(count(&env.fake, "agent.prompt"), 0);
}

#[tokio::test]
async fn a_failed_nudge_still_delivers_and_does_not_set_the_marker() {
    let env = env().await;
    env.fake.on_seq(
        "agent.prompt",
        vec![fake_herdr::Script::error("agent_blocked", "no")],
    );
    let (sender, _) = pair(&env).await;
    let out = sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": "x"}))
        .await;
    assert_eq!(out["delivered"], true);
    assert_eq!(out["nudged"], false);
    assert!(out["nudge_skipped"].is_string());
    assert!(!inbox(&env, "w1:p4").join(".nudged").exists());
}

#[tokio::test]
async fn nudges_coalesce_until_the_inbox_is_read() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    let first = sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": "one"}))
        .await;
    assert_eq!(first["nudged"], true);
    assert!(inbox(&env, "w1:p4").join(".nudged").exists());

    let second = sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": "two"}))
        .await;
    assert_eq!(second["nudged"], false);
    assert_eq!(second["nudge_skipped"], "already_nudged");
    assert_eq!(count(&env.fake, "agent.prompt"), 1);

    reader.ok("herdr_mail_read", json!({})).await;
    assert!(!inbox(&env, "w1:p4").join(".nudged").exists());

    let third = sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": "three"}))
        .await;
    assert_eq!(third["nudged"], true);
    assert_eq!(count(&env.fake, "agent.prompt"), 2);
}

#[tokio::test]
async fn read_limit_and_remaining_and_mark_read_false() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    for n in 0..3 {
        sender
            .ok(
                "herdr_mail_send",
                json!({"to": "rev", "body": format!("m{n}"), "notify": false}),
            )
            .await;
    }
    let peek = reader
        .ok("herdr_mail_read", json!({"mark_read": false, "limit": 2}))
        .await;
    assert_eq!(peek["messages"].as_array().unwrap().len(), 2);
    assert_eq!(peek["remaining"], 1);
    assert_eq!(files(&inbox(&env, "w1:p4").join("new")).len(), 3);

    let first = reader.ok("herdr_mail_read", json!({"limit": 2})).await;
    assert_eq!(first["messages"][0]["body"], "m0");
    assert_eq!(first["messages"][1]["body"], "m1");
    assert_eq!(first["remaining"], 1);
    assert_eq!(files(&inbox(&env, "w1:p4").join("new")).len(), 1);
    assert_eq!(files(&inbox(&env, "w1:p4").join("read")).len(), 2);

    let rest = reader.ok("herdr_mail_read", json!({})).await;
    assert_eq!(rest["messages"][0]["body"], "m2");
    assert_eq!(rest["remaining"], 0);
}

#[tokio::test]
async fn read_deletes_read_files_older_than_24_hours() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "x", "notify": false}),
        )
        .await;
    reader.ok("herdr_mail_read", json!({})).await;
    let read_dir = inbox(&env, "w1:p4").join("read");
    let kept = read_dir.join("fresh.json");
    let stale = read_dir.join("stale.json");
    fs::write(&kept, "{}").unwrap();
    fs::write(&stale, "{}").unwrap();
    let old = SystemTime::now() - Duration::from_secs(25 * 3600);
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    reader.ok("herdr_mail_read", json!({})).await;
    assert!(kept.exists());
    assert!(!stale.exists());
}

#[tokio::test]
async fn read_and_wait_need_a_verified_caller() {
    let env = env().await;
    let external = bridge(&env, ToolMode::Read, None, 1).await;
    let liar = bridge(&env, ToolMode::Read, Some("w1:p4"), 777).await;
    for client in [&external, &liar] {
        for (tool, args) in [
            ("herdr_mail_read", json!({})),
            ("herdr_mail_wait", json!({"timeout_ms": 100})),
        ] {
            let result = client.call(tool, args).await;
            assert_eq!(result.is_error, Some(true), "{tool}");
            let body = result.structured_content.unwrap();
            assert_eq!(body["code"], "no_caller", "{tool}");
            assert!(body["message"].as_str().unwrap().contains("pane"));
        }
    }
}

#[tokio::test]
async fn mail_wait_returns_when_a_message_arrives_mid_wait() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    let started = Instant::now();
    let waiting = reader.ok("herdr_mail_wait", json!({"timeout_ms": 4000}));
    let sending = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        sender
            .ok(
                "herdr_mail_send",
                json!({"to": "rev", "body": "x", "notify": false}),
            )
            .await;
    };
    let (out, ()) = tokio::join!(waiting, sending);
    assert_eq!(out["matched"], true);
    assert_eq!(out["timed_out"], false);
    assert_eq!(out["count"], 1);
    assert!(started.elapsed() < Duration::from_millis(3000));
    // Waiting does not consume.
    let read = reader.ok("herdr_mail_read", json!({})).await;
    assert_eq!(read["messages"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn mail_wait_returns_at_once_with_unread_mail_and_times_out_without() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    let started = Instant::now();
    let out = reader
        .ok("herdr_mail_wait", json!({"timeout_ms": 400}))
        .await;
    assert_eq!(
        out,
        json!({"matched": false, "timed_out": true, "count": 0})
    );
    assert!(started.elapsed() >= Duration::from_millis(350));

    sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "x", "notify": false}),
        )
        .await;
    let started = Instant::now();
    let out = reader
        .ok("herdr_mail_wait", json!({"timeout_ms": 4000}))
        .await;
    assert_eq!(out["matched"], true);
    assert!(started.elapsed() < Duration::from_millis(400));
}

#[tokio::test]
async fn mail_wait_stops_when_cancelled() {
    let env = env().await;
    let mailbox = herdr_mcp::mailbox::Mailbox::new(env.state.path(), env.fake.path());
    let ct = CancellationToken::new();
    let canceller = ct.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let end = wait_for_mail(
        &mailbox,
        "w1:p4",
        Duration::from_secs(30),
        Duration::from_millis(20),
        &ct,
        None::<&tokio::sync::mpsc::UnboundedSender<Progress>>,
    )
    .await;
    assert!(matches!(end, WaitEnd::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn bodies_over_16_kib_are_refused() {
    let env = env().await;
    let (sender, _) = pair(&env).await;
    let big = "a".repeat(16 * 1024 + 1);
    assert_eq!(
        sender
            .err_code(
                "herdr_mail_send",
                json!({"to": "rev", "body": big, "notify": false})
            )
            .await,
        "body_too_large"
    );
    assert!(files(&inbox(&env, "w1:p4").join("new")).is_empty());
    let exact = "a".repeat(16 * 1024);
    sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": exact, "notify": false}),
        )
        .await;
}

#[tokio::test]
async fn an_inbox_holds_at_most_500_unread() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "seed", "notify": false}),
        )
        .await;
    let new = inbox(&env, "w1:p4").join("new");
    for n in 0..499 {
        fs::write(
            new.join(format!("0000000000000-0000000001-{n:06}.json")),
            "{}",
        )
        .unwrap();
    }
    assert_eq!(files(&new).len(), 500);
    assert_eq!(
        sender
            .err_code(
                "herdr_mail_send",
                json!({"to": "rev", "body": "x", "notify": false})
            )
            .await,
        "inbox_full"
    );
    // Reading frees room.
    reader.ok("herdr_mail_read", json!({"limit": 1})).await;
    sender
        .ok(
            "herdr_mail_send",
            json!({"to": "rev", "body": "x", "notify": false}),
        )
        .await;
}

#[tokio::test]
async fn directories_are_0700_and_files_0600() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": "x"}))
        .await;
    let root = inbox(&env, "w1:p4");
    for dir in [
        root.parent().unwrap().to_path_buf(),
        root.clone(),
        root.join("new"),
        root.join("read"),
    ] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    let message = files(&root.join("new")).pop().unwrap();
    assert_eq!(mode(&message), 0o600);
    assert_eq!(mode(&root.join(".nudged")), 0o600);
    reader.ok("herdr_mail_read", json!({})).await;
    let moved = files(&root.join("read")).pop().unwrap();
    assert_eq!(mode(&moved), 0o600);
}

#[tokio::test]
async fn two_concurrent_senders_lose_nothing() {
    let env = env().await;
    let a = bridge(&env, ToolMode::Control, Some("w1:p1"), 4242).await;
    let b = bridge(&env, ToolMode::Control, Some("w1:p1"), 4242).await;
    let reader = bridge(&env, ToolMode::Control, Some("w1:p4"), 5555).await;
    let fa = a.ok(
        "herdr_mail_send",
        json!({"to": "rev", "body": "from-a", "notify": false}),
    );
    let fb = b.ok(
        "herdr_mail_send",
        json!({"to": "rev", "body": "from-b", "notify": false}),
    );
    let (x, y) = tokio::join!(fa, fb);
    assert_ne!(x["id"], y["id"]);
    assert_eq!(files(&inbox(&env, "w1:p4").join("new")).len(), 2);
    let read = reader.ok("herdr_mail_read", json!({})).await;
    let mut bodies: Vec<&str> = read["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    bodies.sort();
    assert_eq!(bodies, ["from-a", "from-b"]);
}

#[tokio::test]
async fn read_mode_has_the_mail_tools_with_the_right_annotations() {
    let env = env().await;
    let h = bridge(&env, ToolMode::Read, Some("w1:p1"), 4242).await;
    let tools = h.client.list_all_tools().await.unwrap();
    let find = |name: &str| tools.iter().find(|t| t.name == name).expect(name);
    for name in ["herdr_mail_read", "herdr_mail_wait"] {
        let ann = find(name).annotations.as_ref().unwrap();
        assert_eq!(ann.read_only_hint, Some(true), "{name}");
    }
    let ann = find("herdr_mail_send").annotations.as_ref().unwrap();
    assert_eq!(ann.destructive_hint, Some(false));
    assert_ne!(ann.read_only_hint, Some(true));
}

#[tokio::test]
async fn logs_never_contain_a_message_body() {
    let env = env().await;
    let (sender, reader) = pair(&env).await;
    sender
        .ok("herdr_mail_send", json!({"to": "rev", "body": SENTINEL}))
        .await;
    reader.ok("herdr_mail_read", json!({})).await;
    sender
        .err_code("herdr_mail_send", json!({"to": "nobody", "body": SENTINEL}))
        .await;
    let mut seen = 0;
    for entry in fs::read_dir(env.logs.path().join("logs")).unwrap() {
        let text = fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains(SENTINEL));
        seen += text.lines().count();
    }
    assert!(seen >= 3);
}
