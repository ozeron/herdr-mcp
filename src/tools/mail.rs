//! The agent mailbox tools. Message bodies are never logged.

use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::guard::TargetKind;
use crate::herdr::{AgentPromptParams, AgentStatus, Client, HerdrError};
use crate::mailbox::{self, Inbox, Mailbox, Message, Party, SenderFilter};
use crate::scope::{Scope, Scoped, TargetRequest};
use crate::server::Inner;
use crate::tools::{DEFAULT_TIMEOUT, clamp_timeout};
use crate::wait::{PROGRESS_INTERVAL, Progress};

pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_READ_LIMIT: u32 = 50;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SendParams {
    /// Pane ID or agent name of the recipient. A name with no live agent is delivered to its name inbox anyway, so a respawned agent of that name gets it.
    pub to: String,
    /// The message text, at most 16 KiB.
    pub body: String,
    /// The ID of the message this answers.
    pub reply_to: Option<String>,
    /// Nudge an idle or done agent recipient to read its mail (default true). The nudge never carries the body.
    pub notify: Option<bool>,
    /// Allow mailing your own pane. Only when the user asked for it.
    pub allow_self: Option<bool>,
    /// Remote callers only: the inbox replies should go to, a slug of lowercase letters, digits and dashes. Default: one per client (`remote-chatgpt`, `remote-claude`), shown by herdr_capabilities as `inbox`. Pick another to keep separate work apart.
    pub inbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadParams {
    /// Only mail from this sender (agent name or pane ID); other mail stays unread.
    pub from: Option<String>,
    /// Move the returned messages out of the unread inbox (default true).
    pub mark_read: Option<bool>,
    /// At most this many messages, oldest first (default 50).
    pub limit: Option<u32>,
    /// Remote callers only: the inbox to use, a slug of lowercase letters, digits and dashes. Default: one per client (`remote-chatgpt`, `remote-claude`), shown by herdr_capabilities as `inbox`. Pick another to keep separate work apart.
    pub inbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WaitParams {
    /// Only wait for mail from this sender (agent name or pane ID).
    pub from: Option<String>,
    /// How long to wait, in milliseconds. Default 30000, capped at the wait cap.
    pub timeout_ms: Option<u64>,
    /// Remote callers only: the inbox to use, a slug of lowercase letters, digits and dashes. Default: one per client (`remote-chatgpt`, `remote-claude`), shown by herdr_capabilities as `inbox`. Pick another to keep separate work apart.
    pub inbox: Option<String>,
}

#[derive(Debug)]
pub enum WaitEnd {
    Mail(usize),
    TimedOut,
    Cancelled,
}

const MAX_SLUG: usize = 40;

/// The name inbox a remote client reads, and the sender name its mail carries: `requested` when
/// given, else one per client product, so ChatGPT and a remote Claude never share one. Name
/// inboxes are files, so the inbox outlives the bridge and the chat.
pub fn remote_inbox(client: Option<&str>, requested: Option<&str>) -> Result<String, HerdrError> {
    if let Some(slug) = requested {
        let valid = !slug.is_empty()
            && slug.len() <= MAX_SLUG
            && slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        return if valid {
            Ok(slug.to_owned())
        } else {
            Err(HerdrError::invalid_params(
                "inbox is a slug: lowercase letters, digits and dashes, at most 40, starting with a letter or digit",
            ))
        };
    }
    let client = client.unwrap_or_default().to_lowercase();
    // `remote-` keeps a default inbox apart from a local agent's name inbox, such as an agent
    // named claude.
    if client.contains("openai") || client.contains("chatgpt") {
        return Ok("remote-chatgpt".into());
    }
    if client.contains("claude") || client.contains("anthropic") {
        return Ok("remote-claude".into());
    }
    let slug: String = client
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = format!("remote-{slug}").chars().take(MAX_SLUG).collect();
    Ok(slug.trim_end_matches('-').to_owned())
}

/// The remote inbox for this call, refused when a live local agent already has that name: its
/// name inbox would be shared.
async fn checked_remote_inbox(
    inner: &Inner,
    client: Option<&str>,
    requested: Option<&str>,
) -> Result<String, HerdrError> {
    let slug = remote_inbox(client, requested)?;
    let (herdr, _) = inner.session()?;
    match herdr.agent_get(&slug).await {
        Ok(agent) => Err(HerdrError::detailed(
            "inbox_taken",
            format!(
                "a local agent is named {slug:?}, so that inbox is its own; pass another inbox"
            ),
            json!({"inbox": slug, "pane": agent.pane_id}),
        )),
        Err(err) if err.is_target_gone() => Ok(slug),
        Err(err) => Err(err),
    }
}

fn local_inbox_param(requested: Option<&str>) -> Result<(), HerdrError> {
    match requested {
        Some(_) => Err(HerdrError::invalid_params(
            "inbox is for remote callers; a local caller's inbox is its pane and agent name",
        )),
        None => Ok(()),
    }
}

fn no_caller(tool: &str) -> HerdrError {
    HerdrError::tool(
        "no_caller",
        format!(
            "{tool} needs a verified caller: the inbox is tied to the caller's Herdr pane, and none could be verified"
        ),
    )
}

/// Keeps a free-text agent name from reshaping the fixed nudge sentence.
fn label(raw: &str) -> String {
    raw.chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub async fn send(
    inner: &Inner,
    mailbox: &Mailbox,
    client: Option<&str>,
    params: SendParams,
) -> Result<Value, HerdrError> {
    mailbox::check_body(&params.body)?;
    let remote_from = if inner.is_remote() {
        Some(checked_remote_inbox(inner, client, params.inbox.as_deref()).await?)
    } else {
        local_inbox_param(params.inbox.as_deref())?;
        None
    };
    let mut scope = Scope::open(inner)?;
    let request = TargetRequest::new(
        &params.to,
        &[TargetKind::Pane],
        "herdr_mail_send takes a pane ID or an agent name",
    )
    .allow_self(params.allow_self.unwrap_or(false));
    let Scoped {
        resolution,
        snapshot,
        target,
    } = scope.target_or_none(request).await?;
    let client = scope.client();
    let agent_name = |pane: &str| {
        snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane)
            .and_then(|agent| agent.name.clone())
    };
    let from = match resolution
        .caller
        .as_ref()
        .filter(|_| resolution.caller_verified)
    {
        Some(caller) => Party {
            pane: Some(caller.pane_id.clone()),
            name: agent_name(&caller.pane_id),
        },
        None => Party {
            pane: None,
            name: Some(remote_from.unwrap_or_else(|| "external".into())),
        },
    };
    let sender_label = label(
        from.name
            .as_deref()
            .or(from.pane.as_deref())
            .unwrap_or("external"),
    );
    // Without a live agent the recipient is the name itself.
    let (pane, recipient_name) = match target {
        Some(target) => {
            let name = agent_name(&target.id);
            (Some(target.id), name)
        }
        None => (None, Some(params.to.clone())),
    };
    let key = match &recipient_name {
        Some(name) => mailbox::name_key(name),
        None => mailbox::recipient_key(pane.as_deref().unwrap_or_default()),
    };
    let message = Message {
        id: mailbox::new_id(),
        ts_ms: mailbox::now_ms(),
        from,
        to: Party {
            pane: pane.clone(),
            name: recipient_name.clone(),
        },
        body: params.body,
        reply_to: params.reply_to,
    };
    mailbox.deliver(&key, &message)?;

    let mut out = json!({
        "id": message.id,
        "to": {"pane": pane, "name": recipient_name},
        "delivered": true,
        "nudged": false,
    });
    let skipped = match &pane {
        None => Some("recipient_offline".to_string()),
        Some(pane) => {
            let is_agent = snapshot.agents.iter().any(|agent| agent.pane_id == *pane);
            if !params.notify.unwrap_or(true) {
                Some("notify_false".to_string())
            } else if !is_agent {
                Some("not_an_agent".to_string())
            } else {
                nudge(client, mailbox, pane, &key, &sender_label).await
            }
        }
    };
    match skipped {
        Some(reason) => out["nudge_skipped"] = json!(reason),
        None => out["nudged"] = json!(true),
    }
    Ok(out)
}

/// `None` when the nudge was sent, else why it was not.
async fn nudge(
    client: &Client,
    mailbox: &Mailbox,
    pane: &str,
    key: &str,
    sender_label: &str,
) -> Option<String> {
    match client.agent_get(pane).await.map(|agent| agent.agent_status) {
        Ok(AgentStatus::Idle | AgentStatus::Done) => {}
        Ok(AgentStatus::Working) => return Some("recipient_working".into()),
        Ok(AgentStatus::Blocked) => return Some("recipient_blocked".into()),
        Ok(AgentStatus::Unknown) => return Some("recipient_status_unknown".into()),
        Err(_) => return Some("recipient_status_unavailable".into()),
    }
    if !mailbox.claim_nudge(key) {
        return Some("already_nudged".into());
    }
    let text = format!(
        "New herdr mail from {sender_label}. Read it with the herdr_mail_read tool; reply with herdr_mail_send if a reply is needed."
    );
    match client
        .agent_prompt(&AgentPromptParams::new(pane, text))
        .await
    {
        Ok(_) => None,
        Err(_) => {
            mailbox.release_nudge(key);
            Some("nudge_failed".into())
        }
    }
}

/// The verified caller's pane, with the inbox it reads: its pane inbox plus its name inbox when
/// `agent.get` says the pane holds a named agent.
async fn caller_inbox(
    inner: &Inner,
    tool: &str,
    client: Option<&str>,
    requested: Option<&str>,
) -> Result<Inbox, HerdrError> {
    if inner.is_remote() {
        return Ok(Inbox::named(
            &checked_remote_inbox(inner, client, requested).await?,
        ));
    }
    local_inbox_param(requested)?;
    let mut scope = Scope::open(inner)?;
    let resolution = scope.caller().await?;
    let pane = match &resolution.caller {
        Some(caller) if resolution.caller_verified => caller.pane_id.clone(),
        _ => return Err(no_caller(tool)),
    };
    let name = match scope.client().agent_get(&pane).await {
        Ok(agent) => agent.name,
        Err(err) if err.is_target_gone() => None,
        Err(err) => return Err(err),
    };
    Ok(Inbox::new(&pane, name.as_deref()))
}

fn sender_filter(from: Option<String>) -> Option<SenderFilter> {
    from.filter(|from| !from.is_empty())
        .map(|from| SenderFilter::new([from]))
}

pub async fn read(
    inner: &Inner,
    mailbox: &Mailbox,
    client: Option<&str>,
    params: ReadParams,
) -> Result<Value, HerdrError> {
    let inbox = caller_inbox(inner, "herdr_mail_read", client, params.inbox.as_deref()).await?;
    let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT).max(1) as usize;
    let filter = sender_filter(params.from);
    let (messages, remaining) = mailbox.take(
        &inbox,
        filter.as_ref(),
        params.mark_read.unwrap_or(true),
        limit,
    )?;
    Ok(json!({"messages": messages, "remaining": remaining}))
}

pub async fn wait(
    inner: &Inner,
    mailbox: &Mailbox,
    client_name: Option<&str>,
    ct: &CancellationToken,
    progress: Option<UnboundedSender<Progress>>,
    params: WaitParams,
) -> Result<Value, HerdrError> {
    let inbox = caller_inbox(
        inner,
        "herdr_mail_wait",
        client_name,
        params.inbox.as_deref(),
    )
    .await?;
    let cap = inner.profile.wait_cap(client_name);
    let (timeout, note) = clamp_timeout(params.timeout_ms, cap, DEFAULT_TIMEOUT);
    let filter = sender_filter(params.from);
    let end = wait_for_mail(
        mailbox,
        &inbox,
        filter.as_ref(),
        timeout,
        POLL_INTERVAL,
        ct,
        progress.as_ref(),
    )
    .await;
    let mut out = match end {
        WaitEnd::Mail(count) => json!({"matched": true, "timed_out": false, "count": count}),
        WaitEnd::TimedOut => json!({"matched": false, "timed_out": true, "count": 0}),
        WaitEnd::Cancelled => {
            return Err(HerdrError::tool(
                "cancelled",
                "the client cancelled the wait",
            ));
        }
    };
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

/// Polls the inbox until it holds unread mail (from `from`, when given), `timeout` passes or
/// `ct` is cancelled. It does not consume anything.
pub async fn wait_for_mail(
    mailbox: &Mailbox,
    inbox: &Inbox,
    from: Option<&SenderFilter>,
    timeout: Duration,
    poll: Duration,
    ct: &CancellationToken,
    progress: Option<&UnboundedSender<Progress>>,
) -> WaitEnd {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut last_tick = started;
    loop {
        let count = mailbox.unread_count(inbox, from);
        if count > 0 {
            return WaitEnd::Mail(count);
        }
        let now = Instant::now();
        if now >= deadline {
            return WaitEnd::TimedOut;
        }
        if let Some(progress) = progress
            && now.duration_since(last_tick) >= PROGRESS_INTERVAL
        {
            last_tick = now;
            let _ = progress.send(Progress {
                elapsed: now.duration_since(started),
                cap: timeout,
            });
        }
        tokio::select! {
            () = sleep(poll.min(deadline - now)) => {}
            () = ct.cancelled() => return WaitEnd::Cancelled,
        }
    }
}
