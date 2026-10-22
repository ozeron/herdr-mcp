//! Reply-by-mail for a launched child: the plan, the instruction the child is given, taking its
//! mail, and the combined wait for the child's mail and its turn.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::caller::{Caller, Resolution};
use crate::herdr::{AgentInfo, AgentStatus, Client, HerdrError, SessionSnapshot};
use crate::mailbox::{Inbox, Mailbox, Message, SenderFilter};
use crate::turn::Turn;
use crate::wait::{self, Condition, Detail, Outcome, Progress, Request};

pub const NO_MAIL_NOTE: &str = "child finished without mailing a result; read its pane";

/// What a `reply: mail` launch needs of the caller: the inbox the child's mail lands in and the
/// name the child is told to mail.
#[derive(Debug)]
pub struct MailPlan {
    pub inbox: Inbox,
    pub parent: String,
}

/// Replies go to the caller's inbox, so the caller has to be verified.
pub fn require_verified_caller(resolution: &Resolution) -> Result<&Caller, HerdrError> {
    match &resolution.caller {
        Some(caller) if resolution.caller_verified => Ok(caller),
        _ => Err(HerdrError::invalid_params(
            "reply mail needs a verified caller: replies go to the caller's inbox, and none could be verified",
        )),
    }
}

/// The verified local caller's plan: its pane inbox, under its agent name when it has one.
pub fn caller_plan(
    snapshot: &SessionSnapshot,
    resolution: &Resolution,
) -> Result<MailPlan, HerdrError> {
    let caller = require_verified_caller(resolution)?;
    let name = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == caller.pane_id)
        .and_then(|agent| agent.name.clone());
    Ok(MailPlan {
        inbox: Inbox::new(&caller.pane_id, name.as_deref()),
        parent: name.unwrap_or_else(|| caller.pane_id.clone()),
    })
}

/// A remote caller's plan: the child mails the caller's name inbox, `slug`.
pub fn remote_plan(slug: String) -> MailPlan {
    MailPlan {
        inbox: Inbox::named(&slug),
        parent: slug,
    }
}

/// The child needs the mail tool (`child_has_mail_tool`) to reply; checked before anything is
/// created.
pub fn require_mail_tool(kind: &str, child_has_mail_tool: bool) -> Result<(), HerdrError> {
    if child_has_mail_tool {
        return Ok(());
    }
    Err(HerdrError::tool(
        "reply_unsupported",
        format!(
            "kind {kind:?} gets no herdr MCP server here, so the child would have no mail tool to reply with; use reply screen"
        ),
    ))
}

pub fn reply_paragraph(parent: &str) -> String {
    format!(
        "When you have finished, send your complete final result as the body of one herdr_mail_send call to {parent}. The body is the deliverable: exact, complete, nothing else. Do this even if you also print it."
    )
}

/// Consumes the oldest unread mail from the child and nothing else.
pub fn take_reply(
    mailbox: &Mailbox,
    inbox: &Inbox,
    filter: &SenderFilter,
) -> Result<Option<Message>, HerdrError> {
    Ok(mailbox
        .take(inbox, Some(filter), true, 1)?
        .0
        .into_iter()
        .next())
}

pub fn reply_json(message: &Message) -> Value {
    json!({
        "id": message.id, "body": message.body, "from": message.from, "ts_ms": message.ts_ms,
    })
}

pub struct ReplyWait<'a> {
    pub client: &'a Client,
    pub mailbox: &'a Mailbox,
    pub wait: &'a wait::Tuning,
    /// Poll for the child's mail.
    pub mail_poll: Duration,
    /// How long mail may take to arrive after the turn settles.
    pub reply_grace: Duration,
    pub plan: &'a MailPlan,
    pub filter: &'a SenderFilter,
    pub turn: Turn,
    pub name: &'a str,
    pub remaining: Duration,
    pub ticks: Option<UnboundedSender<Progress>>,
    pub cancel: &'a CancellationToken,
    pub details: &'a Value,
}

/// What the status fields of a received reply come from.
#[derive(Debug, Clone)]
pub enum Basis {
    /// Mail mid-turn: the current status, or nothing when `agent.get` failed.
    Running(Option<Box<AgentInfo>>),
    /// Mail inside the grace after the turn settled.
    Settled(Outcome),
    /// Mail found after the turn wait timed out; the outcome keeps its `turn`.
    TimedOut(Outcome),
}

#[derive(Debug, Clone)]
pub enum ReplyOutcome {
    Received {
        reply: Message,
        basis: Basis,
    },
    /// The turn settled and the grace passed without mail.
    SettledNoMail(Outcome),
    /// The wait cap ran out, in the turn wait or during the grace.
    TimedOut {
        wait: Outcome,
        child: String,
    },
    /// Blocked, unknown or working again: the caller has to look.
    Ended(Outcome),
}

impl ReplyOutcome {
    /// The fields to merge into the launch result.
    pub fn to_json(&self) -> Value {
        fn without_reply(wait: &Outcome) -> Value {
            let mut out = wait.to_json();
            out["reply"] = Value::Null;
            out
        }
        match self {
            Self::Received { reply, basis } => {
                let mut out = match basis {
                    Basis::Running(status) => {
                        let mut out = json!({"matched": true, "timed_out": false});
                        if let Some(agent) = status {
                            out["status"] = json!(agent.agent_status);
                            out["state_change_seq"] = json!(agent.state_change_seq);
                            out["completion_seq"] = json!(agent.completion_seq);
                        }
                        out
                    }
                    Basis::Settled(wait) | Basis::TimedOut(wait) => {
                        let mut out = wait.to_json();
                        if let Some(fields) = out.as_object_mut() {
                            fields.remove("excerpt");
                        }
                        out
                    }
                };
                out["reply"] = reply_json(reply);
                out
            }
            Self::SettledNoMail(wait) => {
                let mut wait = wait.clone();
                wait.notes = vec![NO_MAIL_NOTE.into()];
                without_reply(&wait)
            }
            Self::TimedOut { wait, child } => {
                let mut out = without_reply(wait);
                out["rewait"] = rewait(child);
                out
            }
            Self::Ended(wait) => without_reply(wait),
        }
    }
}

fn rewait(name: &str) -> Value {
    json!({"mail": {"from": name}})
}

fn cancelled(details: Value) -> HerdrError {
    HerdrError::detailed(
        "cancelled",
        "the client cancelled the wait; the agent keeps working. Call herdr_mail_wait with from set to the child's name to resume",
        details,
    )
}

fn timed_out(mut wait: Outcome, name: &str) -> ReplyOutcome {
    wait.matched = false;
    wait.timed_out = true;
    ReplyOutcome::TimedOut {
        wait,
        child: name.into(),
    }
}

fn is_settled(outcome: &Outcome) -> bool {
    outcome.matched
        && matches!(
            &outcome.detail,
            Detail::Agent(agent) if matches!(agent.status, AgentStatus::Done | AgentStatus::Idle)
        )
}

/// The child's current status, for a reply that arrived while it was still running.
async fn running_status(client: &Client, pane: &str) -> Option<Box<AgentInfo>> {
    client.agent_get(pane).await.ok().map(Box::new)
}

/// Waits for the child's mail and its turn together. Mail ends the wait at once. A turn that
/// settles gets `reply_grace` for its mail to arrive.
pub async fn await_reply(w: ReplyWait<'_>) -> Result<ReplyOutcome, HerdrError> {
    let deadline = Instant::now() + w.remaining;
    let turn_wait = wait::wait(
        w.client,
        w.wait,
        Request {
            pane: w.turn.pane.clone(),
            condition: Condition::Turn(w.turn.clone()),
            cap: w.remaining,
        },
        w.ticks,
    );
    tokio::pin!(turn_wait);
    // The settled turn's result and when its grace ends.
    let mut settled: Option<(Outcome, Instant)> = None;
    loop {
        if let Some(reply) = take_reply(w.mailbox, &w.plan.inbox, w.filter)? {
            let basis = match settled {
                Some((outcome, _)) => Basis::Settled(outcome),
                None => Basis::Running(running_status(w.client, &w.turn.pane).await),
            };
            return Ok(ReplyOutcome::Received { reply, basis });
        }
        let now = Instant::now();
        if now >= deadline
            && let Some((outcome, _)) = settled.take()
        {
            return Ok(timed_out(outcome, w.name));
        }
        if let Some((outcome, grace_end)) = &settled
            && now >= *grace_end
        {
            return Ok(ReplyOutcome::SettledNoMail(outcome.clone()));
        }
        let boundary = match &settled {
            Some((_, grace_end)) => (*grace_end).min(deadline),
            None => deadline,
        };
        let nap = w.mail_poll.min(boundary.saturating_duration_since(now));
        tokio::select! {
            result = &mut turn_wait, if settled.is_none() => {
                let mut outcome = result?;
                if is_settled(&outcome) {
                    settled = Some((outcome, Instant::now() + w.reply_grace));
                } else if outcome.timed_out {
                    if let Some(reply) = take_reply(w.mailbox, &w.plan.inbox, w.filter)? {
                        outcome.matched = true;
                        outcome.timed_out = false;
                        return Ok(ReplyOutcome::Received { reply, basis: Basis::TimedOut(outcome) });
                    }
                    return Ok(timed_out(outcome, w.name));
                } else {
                    return Ok(ReplyOutcome::Ended(outcome));
                }
            }
            () = sleep(nap) => {}
            () = w.cancel.cancelled() => {
                let mut details = w.details.clone();
                details["rewait"] = rewait(w.name);
                return Err(cancelled(details));
            }
        }
    }
}
