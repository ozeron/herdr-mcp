#[path = "support/fake_herdr.rs"]
mod fake_herdr;

use fake_herdr::FakeHerdr;
use herdr_mcp::caller::{CallerEnv, StaticAncestry, resolve_caller};
use herdr_mcp::herdr::Client;
use serde_json::{Value, json};

fn pane(id: &str) -> Value {
    json!({
        "pane_id": id, "terminal_id": format!("t-{id}"), "workspace_id": "w1",
        "tab_id": "w1:t1", "focused": false, "agent_status": "idle", "revision": 1
    })
}

fn snapshot(panes: &[&str]) -> Value {
    json!({"type": "session_snapshot", "snapshot": {
        "version": "0.9.3", "protocol": 22, "workspaces": [], "tabs": [],
        "panes": panes.iter().map(|p| pane(p)).collect::<Vec<_>>(),
        "layouts": [], "agents": []
    }})
}

fn process_info(pane_id: &str, shell_pid: u32) -> Value {
    json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": shell_pid,
        "foreground_process_group_id": shell_pid, "tty": "/dev/ttys001",
        "foreground_processes": []
    }})
}

fn env(pane_id: Option<&str>) -> CallerEnv {
    CallerEnv {
        pane_id: pane_id.map(Into::into),
        tab_id: pane_id.map(|_| "w1:t1".into()),
        workspace_id: pane_id.map(|_| "w1".into()),
    }
}

fn client(fake: &FakeHerdr) -> Client {
    Client::new(fake.path()).unwrap()
}

#[tokio::test]
async fn env_pane_whose_shell_is_an_ancestor_is_verified() {
    let fake = FakeHerdr::start().await;
    fake.reply("pane.process_info", process_info("w1:p1", 4242));
    let ancestry = StaticAncestry(vec![9000, 4242, 1]);

    let resolution = resolve_caller(&client(&fake), &env(Some("w1:p1")), &ancestry)
        .await
        .unwrap();

    let caller = resolution.caller.unwrap();
    assert_eq!(caller.pane_id, "w1:p1");
    assert_eq!(caller.tab_id.as_deref(), Some("w1:t1"));
    assert_eq!(caller.workspace_id.as_deref(), Some("w1"));
    assert!(resolution.caller_verified);
    assert_eq!(resolution.guard_target.as_deref(), Some("w1:p1"));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn env_pane_with_foreign_shell_is_unverified_and_guards_the_env_pane() {
    let fake = FakeHerdr::start().await;
    fake.reply("pane.process_info", process_info("w1:p2", 5555));
    fake.reply("session.snapshot", snapshot(&["w1:p1", "w1:p2"]));
    fake.reply("pane.process_info", process_info("w1:p1", 4444));
    fake.reply("pane.process_info", process_info("w1:p2", 5555));
    let ancestry = StaticAncestry(vec![9000, 4242, 1]);

    let resolution = resolve_caller(&client(&fake), &env(Some("w1:p2")), &ancestry)
        .await
        .unwrap();

    assert!(!resolution.caller_verified);
    assert_eq!(resolution.guard_target.as_deref(), Some("w1:p2"));
    assert_eq!(resolution.caller.unwrap().pane_id, "w1:p2");
}

#[tokio::test]
async fn no_env_falls_back_to_the_ancestor_scan() {
    let fake = FakeHerdr::start().await;
    fake.reply("session.snapshot", snapshot(&["w1:p1", "w1:p2", "w1:p3"]));
    fake.reply("pane.process_info", process_info("w1:p1", 100));
    fake.reply("pane.process_info", process_info("w1:p2", 200));
    fake.reply("pane.process_info", process_info("w1:p3", 300));
    let ancestry = StaticAncestry(vec![9000, 300, 1]);

    let resolution = resolve_caller(&client(&fake), &env(None), &ancestry)
        .await
        .unwrap();

    let caller = resolution.caller.unwrap();
    assert_eq!(caller.pane_id, "w1:p3");
    assert_eq!(caller.workspace_id.as_deref(), Some("w1"));
    assert!(resolution.caller_verified);
    assert_eq!(resolution.guard_target.as_deref(), Some("w1:p3"));
}

#[tokio::test]
async fn no_env_and_no_match_reports_no_caller() {
    let fake = FakeHerdr::start().await;
    fake.reply("session.snapshot", snapshot(&["w1:p1"]));
    fake.reply("pane.process_info", process_info("w1:p1", 100));
    let ancestry = StaticAncestry(vec![9000, 1]);

    let resolution = resolve_caller(&client(&fake), &env(None), &ancestry)
        .await
        .unwrap();

    assert!(resolution.caller.is_none());
    assert!(!resolution.caller_verified);
    assert!(resolution.guard_target.is_none());
}

#[tokio::test]
async fn pane_without_a_shell_pid_never_matches() {
    let fake = FakeHerdr::start().await;
    fake.reply(
        "pane.process_info",
        json!({"type": "pane_process_info", "process_info": {
            "pane_id": "w1:p1", "foreground_processes": []}}),
    );
    fake.reply("session.snapshot", snapshot(&[]));

    let resolution = resolve_caller(
        &client(&fake),
        &env(Some("w1:p1")),
        &StaticAncestry(vec![0, 1]),
    )
    .await
    .unwrap();

    assert!(!resolution.caller_verified);
    assert_eq!(resolution.guard_target.as_deref(), Some("w1:p1"));
}

#[tokio::test]
async fn herdr_not_running_is_an_error() {
    let fake = FakeHerdr::start().await;
    let path = fake.path().to_path_buf();
    drop(fake);
    let client = Client::new(&path).unwrap();

    let err = resolve_caller(&client, &env(None), &StaticAncestry(vec![]))
        .await
        .unwrap_err();

    assert_eq!(err.code(), "herdr_not_running");
}

#[test]
fn real_ancestry_contains_the_parent_process() {
    use herdr_mcp::caller::{Ancestry, ProcessAncestry};
    let ancestors = ProcessAncestry.ancestor_pids();
    assert!(ancestors.contains(&std::os::unix::process::parent_id()));
    assert!(!ancestors.contains(&0));
}
