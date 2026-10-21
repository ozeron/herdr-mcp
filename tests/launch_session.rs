#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use fake_herdr::FakeHerdr;
use harness::{count, ok_reply, requests};
use herdr_mcp::herdr::{Client, HerdrError};
use herdr_mcp::place::{Created, Placed};
use herdr_mcp::tools::launch_session::{LaunchSession, Phase};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn placed(created: Option<Created>) -> Placed {
    Placed {
        pane_id: "w1:p9".into(),
        tab_id: "w1:t9".into(),
        workspace_id: "w1".into(),
        created,
    }
}

fn injection_error() -> HerdrError {
    HerdrError::tool("bridge_binary_unknown", "cannot locate this binary")
}

#[tokio::test]
async fn fail_and_clean_up_closes_the_created_tab_and_returns_the_error_unchanged() {
    let fake = FakeHerdr::start().await;
    fake.reply("tab.close", ok_reply());
    let client = Client::new(fake.path()).unwrap();
    let session = LaunchSession::new(&client, placed(Some(Created::Tab("w1:t9".into()))), "kid");

    let err = session.fail_and_clean_up(injection_error()).await;

    assert_eq!(err.code(), "bridge_binary_unknown");
    assert!(err.details().is_none());
    let closed = requests(&fake, "tab.close");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["params"]["tab_id"], "w1:t9");
}

#[tokio::test]
async fn fail_and_clean_up_closes_nothing_in_a_pane_the_call_did_not_create() {
    let fake = FakeHerdr::start().await;
    let client = Client::new(fake.path()).unwrap();
    let session = LaunchSession::new(&client, placed(None), "kid");

    let err = session.fail_and_clean_up(injection_error()).await;

    assert_eq!(err.code(), "bridge_binary_unknown");
    for method in ["pane.close", "tab.close", "workspace.close"] {
        assert_eq!(count(&fake, method), 0, "{method}");
    }
}

#[tokio::test]
async fn fail_keep_adds_the_phase_details_to_a_tool_error_and_closes_nothing() {
    let fake = FakeHerdr::start().await;
    let client = Client::new(fake.path()).unwrap();
    let mut session =
        LaunchSession::new(&client, placed(Some(Created::Tab("w1:t9".into()))), "kid");
    session.advance(Phase::Started);

    let err = session.fail_keep(injection_error());

    let details = err.details().expect("details");
    assert_eq!(details["phase"], "started");
    assert_eq!(details["name"], "kid");
    assert_eq!(details["pane"], "w1:p9");
    assert_eq!(details["created"], json!({"kind": "tab", "id": "w1:t9"}));
    assert_eq!(count(&fake, "tab.close"), 0);
}

#[tokio::test]
async fn a_cancelled_race_keeps_the_pane_and_reports_the_current_phase() {
    let fake = FakeHerdr::start().await;
    let client = Client::new(fake.path()).unwrap();
    let session = LaunchSession::new(&client, placed(Some(Created::Tab("w1:t9".into()))), "kid");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = session
        .race(&cancel, std::future::pending::<()>())
        .await
        .unwrap_err();

    assert_eq!(err.code(), "cancelled");
    assert_eq!(err.details().unwrap()["phase"], "placed");
    assert_eq!(count(&fake, "tab.close"), 0);
}
