#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::time::Duration;

use fake_herdr::FakeHerdr;
use harness::{count, snapshot};
use herdr_mcp::caller::{Caller, Resolution};
use herdr_mcp::delegation::{
    Basis, MailPlan, ReplyOutcome, ReplyWait, await_reply, mail_plan, reply_paragraph,
};
use herdr_mcp::herdr::{Client, SessionSnapshot};
use herdr_mcp::mailbox::{
    Inbox, Mailbox, Message, Party, SenderFilter, new_id, now_ms, recipient_key,
};
use herdr_mcp::turn::{Turn, TurnFrom};
use herdr_mcp::wait::Tuning;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const PANE: &str = "w1:p9";
const TERMINAL: &str = "t-w1:p9";

fn tuning() -> Tuning {
    Tuning {
        poll_interval: Duration::from_millis(20),
        progress_interval: Duration::from_secs(60),
        observe_window: Duration::from_millis(150),
        recheck_delay: Duration::from_millis(30),
        pane_interval: Duration::from_millis(10),
    }
}

fn info(status: &str, seq: u64, completion: Option<u64>) -> Value {
    json!({"type": "agent_info", "agent": {
        "terminal_id": TERMINAL, "name": "kid", "agent": "codex", "agent_status": status,
        "pane_id": PANE, "tab_id": "w1:t2", "workspace_id": "w1", "focused": false,
        "revision": 1, "state_change_seq": seq, "completion_seq": completion,
        "interactive_ready": true, "launch_pending": false,
    }})
}

fn read_result(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:t2", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 7, "truncated": false}})
}

fn turn() -> Turn {
    Turn {
        pane: PANE.into(),
        terminal_id: TERMINAL.into(),
        shell_pid: None,
        after: 41,
        from: TurnFrom::Working,
    }
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

struct Rig {
    fake: FakeHerdr,
    dir: TempDir,
    plan: MailPlan,
    filter: SenderFilter,
    cancel: CancellationToken,
    details: Value,
}

impl Rig {
    async fn new() -> Self {
        let fake = FakeHerdr::start().await;
        fake.reply_seq("pane.read", vec![read_result("child output\n")]);
        Self {
            fake,
            dir: tempfile::tempdir().unwrap(),
            plan: MailPlan {
                inbox: Inbox::new("w1:p1", None),
                parent: "w1:p1".into(),
            },
            filter: SenderFilter::new([PANE.to_string(), "kid".to_string()]),
            cancel: CancellationToken::new(),
            details: json!({"phase": "waiting", "pane": PANE}),
        }
    }

    fn agent(&self, results: Vec<Value>) {
        self.fake.reply_seq("agent.get", results);
    }

    fn mailbox(&self) -> Mailbox {
        Mailbox::new(self.dir.path(), self.fake.path())
    }

    fn deliver(&self, message: &Message) {
        self.mailbox()
            .deliver(&recipient_key("w1:p1"), message)
            .unwrap();
    }

    fn unread(&self) -> usize {
        self.mailbox().unread_count(&self.plan.inbox, None)
    }

    async fn reply(
        &self,
        remaining_ms: u64,
        grace_ms: u64,
    ) -> Result<ReplyOutcome, herdr_mcp::herdr::HerdrError> {
        let client = Client::new(self.fake.path()).unwrap();
        let mailbox = self.mailbox();
        await_reply(ReplyWait {
            client: &client,
            mailbox: &mailbox,
            wait: &tuning(),
            mail_poll: Duration::from_millis(10),
            reply_grace: Duration::from_millis(grace_ms),
            plan: &self.plan,
            filter: &self.filter,
            turn: turn(),
            name: "kid",
            remaining: Duration::from_millis(remaining_ms),
            ticks: None,
            cancel: &self.cancel,
            details: &self.details,
        })
        .await
    }
}

#[tokio::test]
async fn mail_mid_turn_is_the_reply_with_the_current_status_and_no_excerpt() {
    let rig = Rig::new().await;
    rig.agent(vec![info("working", 42, None)]);
    rig.deliver(&mail_from(PANE, Some("kid"), "the result"));

    let out = rig.reply(5000, 50).await.unwrap();

    assert!(matches!(
        out,
        ReplyOutcome::Received {
            basis: Basis::Running(Some(_)),
            ..
        }
    ));
    let json = out.to_json();
    assert_eq!(json["matched"], true);
    assert_eq!(json["timed_out"], false);
    assert_eq!(json["status"], "working");
    assert_eq!(json["state_change_seq"], 42);
    assert_eq!(json["reply"]["body"], "the result");
    assert_eq!(json["reply"]["from"]["pane"], PANE);
    assert!(json.get("excerpt").is_none());
    assert!(json.get("pane").is_none());
}

#[tokio::test]
async fn a_turn_that_settles_without_mail_waits_the_grace_then_returns_no_reply() {
    let rig = Rig::new().await;
    rig.agent(vec![info("idle", 43, Some(43))]);

    let started = std::time::Instant::now();
    let out = rig.reply(5000, 150).await.unwrap();

    assert!(started.elapsed() >= Duration::from_millis(150));
    let json = out.to_json();
    assert!(matches!(out, ReplyOutcome::SettledNoMail(_)));
    assert_eq!(json["matched"], true);
    assert_eq!(json["timed_out"], false);
    assert_eq!(json["status"], "idle");
    assert_eq!(json["reply"], Value::Null);
    assert_eq!(json["excerpt"], "child output");
    assert_eq!(
        json["note"],
        "child finished without mailing a result; read its pane"
    );
    assert!(json.get("rewait").is_none());
}

#[tokio::test]
async fn mail_inside_the_grace_is_still_the_reply() {
    let rig = Rig::new().await;
    rig.agent(vec![info("done", 43, Some(43))]);
    let mailbox = rig.mailbox();
    let message = mail_from(PANE, Some("kid"), "late result");
    let later = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        mailbox.deliver(&recipient_key("w1:p1"), &message).unwrap();
    });

    let out = rig.reply(5000, 3000).await.unwrap();
    later.await.unwrap();

    assert!(matches!(
        out,
        ReplyOutcome::Received {
            basis: Basis::Settled(_),
            ..
        }
    ));
    let json = out.to_json();
    assert_eq!(json["reply"]["body"], "late result");
    assert_eq!(json["status"], "done");
    assert_eq!(json["matched"], true);
    assert!(json.get("excerpt").is_none());
}

#[tokio::test]
async fn a_cap_that_runs_out_returns_a_timeout_naming_the_child() {
    let rig = Rig::new().await;
    rig.agent(vec![info("working", 41, None)]);

    let out = rig.reply(200, 50).await.unwrap();

    assert!(matches!(out, ReplyOutcome::TimedOut { .. }));
    let json = out.to_json();
    assert_eq!(json["matched"], false);
    assert_eq!(json["timed_out"], true);
    assert_eq!(json["reply"], Value::Null);
    assert_eq!(json["rewait"], json!({"mail": {"from": "kid"}}));
    assert_eq!(json["turn"]["pane"], PANE);
}

#[tokio::test]
async fn a_cap_that_runs_out_during_the_grace_keeps_the_settled_fields() {
    let rig = Rig::new().await;
    rig.agent(vec![info("idle", 43, Some(43))]);

    let out = rig.reply(300, 10_000).await.unwrap();

    let json = out.to_json();
    assert!(matches!(out, ReplyOutcome::TimedOut { .. }));
    assert_eq!(json["matched"], false);
    assert_eq!(json["timed_out"], true);
    assert_eq!(json["status"], "idle");
    assert_eq!(json["excerpt"], "child output");
    assert_eq!(json["reply"], Value::Null);
    assert_eq!(json["rewait"], json!({"mail": {"from": "kid"}}));
}

#[tokio::test]
async fn a_blocked_turn_returns_no_reply_and_leaves_the_decision_to_the_caller() {
    let rig = Rig::new().await;
    rig.agent(vec![info("blocked", 42, None)]);

    let out = rig.reply(5000, 50).await.unwrap();

    let json = out.to_json();
    assert!(matches!(out, ReplyOutcome::Ended(_)));
    assert_eq!(json["status"], "blocked");
    assert_eq!(json["matched"], true);
    assert_eq!(json["reply"], Value::Null);
    assert_eq!(json["turn"]["pane"], PANE);
    assert!(json.get("rewait").is_none());
}

#[tokio::test]
async fn a_turn_that_is_working_again_returns_no_reply() {
    let rig = Rig::new().await;
    rig.agent(vec![info("working", 45, None)]);

    let out = rig.reply(5000, 50).await.unwrap();

    let json = out.to_json();
    assert!(matches!(out, ReplyOutcome::Ended(_)));
    assert_eq!(json["status"], "working");
    assert_eq!(json["superseded"], true);
    assert_eq!(json["reply"], Value::Null);
}

#[tokio::test]
async fn cancellation_returns_the_cancelled_error_with_the_mail_rewait() {
    let rig = Rig::new().await;
    rig.agent(vec![info("working", 41, None)]);
    let cancel = rig.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let err = rig.reply(5000, 50).await.unwrap_err();

    assert_eq!(err.code(), "cancelled");
    let details = err.details().unwrap();
    assert_eq!(details["rewait"], json!({"mail": {"from": "kid"}}));
    assert_eq!(details["phase"], "waiting");
    assert_eq!(details["pane"], PANE);
}

#[tokio::test]
async fn only_the_childs_mail_is_consumed() {
    let rig = Rig::new().await;
    rig.agent(vec![info("working", 42, None)]);
    rig.deliver(&mail_from("w1:p3", None, "someone else"));
    rig.deliver(&mail_from(PANE, Some("kid"), "the child's"));

    let out = rig.reply(5000, 50).await.unwrap();

    assert_eq!(out.to_json()["reply"]["body"], "the child's");
    assert_eq!(rig.unread(), 1);
    let left = rig
        .mailbox()
        .take(&rig.plan.inbox, None, true, 5)
        .unwrap()
        .0;
    assert_eq!(left[0].body, "someone else");
}

fn resolution(caller: Option<&str>, verified: bool) -> Resolution {
    Resolution {
        caller: caller.map(|pane| Caller {
            pane_id: pane.into(),
            tab_id: Some("w1:t1".into()),
            workspace_id: Some("w1".into()),
        }),
        caller_verified: verified,
        guard_target: caller.map(Into::into),
    }
}

fn snap() -> SessionSnapshot {
    serde_json::from_value(snapshot(None)["snapshot"].clone()).unwrap()
}

#[tokio::test]
async fn the_plan_refuses_an_unverified_caller_before_anything_else() {
    let fake = FakeHerdr::start().await;
    for res in [
        resolution(None, false),
        resolution(Some("w1:p1"), false),
        resolution(None, true),
    ] {
        let err = mail_plan("codex", false, &snap(), &res).unwrap_err();
        assert_eq!(err.code(), "invalid_params");
        assert!(err.to_string().contains("verified caller"), "{err}");
    }
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn the_plan_refuses_a_child_without_the_mail_tool() {
    let err = mail_plan("gemini", false, &snap(), &resolution(Some("w1:p1"), true)).unwrap_err();
    assert_eq!(err.code(), "reply_unsupported");
}

#[tokio::test]
async fn the_plan_mails_the_callers_name_or_pane() {
    let plain = mail_plan("codex", true, &snap(), &resolution(Some("w1:p1"), true)).unwrap();
    assert_eq!(plain.parent, "w1:p1");
    let named = mail_plan("codex", true, &snap(), &resolution(Some("w1:p4"), true)).unwrap();
    assert_eq!(named.parent, "rev");
    assert!(reply_paragraph("rev").contains("herdr_mail_send call to rev."));
}

#[tokio::test]
async fn a_plan_check_touches_nothing_on_the_socket() {
    let rig = Rig::new().await;
    let _ = mail_plan("codex", true, &snap(), &resolution(Some("w1:p1"), true)).unwrap();
    assert_eq!(count(&rig.fake, "agent.get"), 0);
}
