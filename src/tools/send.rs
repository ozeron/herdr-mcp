use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::caller::Resolution;
use crate::delegation::{ReplyWait, await_reply, caller_plan, remote_plan, reply_paragraph};
use crate::guard::TargetKind;
use crate::herdr::{
    AgentInfo, AgentPromptParams, AgentReadParams, AgentStatus, ApiErrorCode, Client, HerdrError,
    Outcome, PaneSendInputParams, PromptWait, ReadSource,
};
use crate::mailbox::{self, SenderFilter};
use crate::provenance;
use crate::providers::provider;
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;
use crate::tools::interrupt;
use crate::tools::launch::{Reply, pending_note, visible_dialog};
use crate::tools::mail;
use crate::tools::{DEFAULT_TIMEOUT, clamp_timeout};
use crate::turn::Turn;
use crate::wait::{self, Condition, Progress, Request};

pub const ACTIVITY_GATE_MS: u64 = 6000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    Auto,
    AfterIdle,
    Steer,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum SendWait {
    /// Wait for the turn the prompt started to settle, bounded by the wait cap.
    Turn,
    /// Return once the prompt is delivered. Default.
    None,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SendParams {
    /// Pane ID or agent name.
    pub target: String,
    /// A prompt for an agent. Starts a turn and returns a turn token for herdr_wait. Not for plain panes.
    pub prompt: Option<String>,
    /// Raw text to type, with no Enter. Combine with keys to submit it.
    pub text: Option<String>,
    /// Raw keys to send, such as ["Enter"] or ["y"].
    pub keys: Option<Vec<String>>,
    /// For a prompt. auto (default) starts a turn on an idle agent. On a working agent it queues the prompt only for kinds that queue typed input (Claude Code: delivery queued, or unconfirmed when the queue marker never shows); any other working agent returns busy at once with nothing typed. after_idle waits for idle, then sends; steer types into the running turn and returns no turn; restart interrupts, waits for idle, then sends.
    pub mode: Option<SendMode>,
    /// For a prompt. turn waits for the turn it started to settle and returns the excerpt, bounded by the wait cap; on timed_out, pass the turn to herdr_wait. none (default) returns once delivered. A queued prompt has no turn yet, so it returns at once.
    pub wait: Option<SendWait>,
    /// For a prompt typed into a working agent that queues it (Claude Code): submit the queued prompt now with the agent's send-now key. Interrupts the agent's current turn flow; use only when the user wants the new prompt handled immediately. Ignored, with a note, for agents without such a key.
    pub send_now: Option<bool>,
    /// For a prompt. screen (default) leaves the answer on the agent's screen. mail asks the agent to mail its exact final answer to you (your pane inbox, or a remote caller's inbox): wait turn returns it as `reply`; otherwise `reply: "pending"` and the mail arrives later. Use it when you need the whole answer, not a screen excerpt. Needs an agent that has the herdr mail tool; one without it cannot reply.
    pub reply: Option<Reply>,
    /// Answer a blocked agent's prompt, or a startup dialog on screen, with text or keys. Only for an agent you launched; refused otherwise.
    pub answer_blocked: Option<bool>,
    /// How long after_idle and restart wait for idle, in milliseconds. Default 30000, capped at the wait cap.
    pub timeout_ms: Option<u64>,
    /// Allow sending to your own pane. Only when the user asked for it.
    pub allow_self: Option<bool>,
}

fn validate(params: &SendParams) -> Result<(), HerdrError> {
    let raw = params.text.is_some() || params.keys.is_some();
    match (&params.prompt, raw) {
        (None, false) => {
            return Err(HerdrError::invalid_params(
                "give prompt, or text and/or keys",
            ));
        }
        (Some(_), true) => {
            return Err(HerdrError::invalid_params(
                "give prompt, or text and/or keys, not both",
            ));
        }
        _ => {}
    }
    if params.prompt.as_deref().is_some_and(str::is_empty)
        || params.text.as_deref().is_some_and(str::is_empty)
        || params.keys.as_ref().is_some_and(Vec::is_empty)
    {
        return Err(HerdrError::invalid_params(
            "prompt, text and keys must not be empty",
        ));
    }
    if raw && params.mode.is_some() {
        return Err(HerdrError::invalid_params(
            "mode applies to a prompt, not to text or keys",
        ));
    }
    if raw && params.wait.is_some() {
        return Err(HerdrError::invalid_params(
            "wait applies to a prompt, not to text or keys",
        ));
    }
    if raw && params.reply.is_some() {
        return Err(HerdrError::invalid_params(
            "reply applies to a prompt, not to text or keys",
        ));
    }
    if raw && params.send_now.is_some() {
        return Err(HerdrError::invalid_params(
            "send_now applies to a prompt, not to text or keys",
        ));
    }
    if params.prompt.is_some() && params.answer_blocked == Some(true) {
        return Err(HerdrError::invalid_params(
            "answer_blocked applies to text or keys, not a prompt",
        ));
    }
    Ok(())
}

pub async fn run(
    inner: &Inner,
    client_info_name: Option<&str>,
    ct: &CancellationToken,
    progress: Option<UnboundedSender<Progress>>,
    params: SendParams,
) -> Result<Value, HerdrError> {
    validate(&params)?;
    let started = Instant::now();
    let mut scope = Scope::open(inner)?;
    let request = TargetRequest::new(
        &params.target,
        &[TargetKind::Pane],
        "herdr_send takes a pane ID or an agent name",
    )
    .allow_self(params.allow_self.unwrap_or(false));
    let scoped = scope.target(request).await?;
    let client = scope.client();
    let resolution = &scoped.resolution;
    let pane = scoped.target.id.as_str();
    let agent = scoped.agent();
    let is_agent = agent.is_some();

    match params.prompt.as_deref() {
        Some(prompt) => {
            if !is_agent {
                return Err(HerdrError::tool(
                    "not_an_agent",
                    format!("{pane} has no agent; send text and keys to a plain pane"),
                ));
            }
            let cap = inner.profile.wait_cap(client_info_name);
            let (timeout, _) = clamp_timeout(params.timeout_ms, cap, DEFAULT_TIMEOUT);
            let mode = params.mode.unwrap_or(SendMode::Auto);
            let pacing = Pacing {
                idle_poll: inner.launch.idle_poll,
                settle: inner.interrupt_settle,
                timeout,
            };
            let send_now = params.send_now.unwrap_or(false);
            let plan = match params.reply.unwrap_or_default() {
                Reply::Screen => None,
                Reply::Mail if inner.is_remote() => Some(remote_plan(
                    mail::checked_remote_inbox(inner, client_info_name, None).await?,
                )),
                Reply::Mail => Some(caller_plan(&scoped.snapshot, resolution)?),
            };
            let with_reply;
            let prompt = match &plan {
                Some(plan) => {
                    with_reply = format!("{prompt}\n\n{}", reply_paragraph(&plan.parent));
                    with_reply.as_str()
                }
                None => prompt,
            };
            let typed_ms = mailbox::now_ms();
            let mut out = send_prompt(
                client,
                pane,
                agent,
                prompt,
                Options { mode, send_now },
                pacing,
                ct,
            )
            .await?;
            let remaining = cap.saturating_sub(started.elapsed());
            let waits = params.wait == Some(SendWait::Turn);
            let turn = serde_json::from_value::<Turn>(out["turn"].clone()).ok();
            match (&plan, turn) {
                (Some(plan), Some(turn)) if waits => {
                    let name = agent.and_then(|agent| agent.name.clone());
                    // Mail sent before the prompt answers something else.
                    let filter =
                        SenderFilter::new(std::iter::once(pane.to_owned()).chain(name.clone()))
                            .since(typed_ms);
                    let outcome = await_reply(ReplyWait {
                        client,
                        mailbox: &inner.mailbox(),
                        wait: &inner.wait,
                        mail_poll: inner.launch.mail_poll,
                        reply_grace: inner.launch.reply_grace,
                        plan,
                        filter: &filter,
                        turn,
                        name: name.as_deref().unwrap_or(pane),
                        remaining,
                        ticks: progress,
                        cancel: ct,
                        details: &json!({"pane": pane}),
                    })
                    .await;
                    match outcome {
                        Ok(outcome) => merge_wait(&mut out, &outcome.to_json()),
                        Err(err) if err.code() == "cancelled" => {
                            out["timed_out"] = json!(true);
                            out["reply"] = json!("pending");
                        }
                        Err(err) => wait_failed(&mut out, &err),
                    }
                }
                (Some(plan), _) => {
                    out["reply"] = json!("pending");
                    add_note(&mut out, &pending_note(inner.is_remote(), &plan.parent));
                }
                (None, _) if waits => {
                    await_turn(client, inner, &mut out, remaining, ct, progress).await;
                }
                (None, _) => {}
            }
            Ok(out)
        }
        None => send_raw(client, resolution, pane, is_agent, &params).await,
    }
}

fn blocked(pane: &str) -> HerdrError {
    HerdrError::tool(
        "agent_blocked",
        format!(
            "{pane} is blocked on a prompt; a new prompt would be typed into it. Read the pane, then answer with keys and answer_blocked, or ask the user"
        ),
    )
}

/// Waits between polls and after an interrupt.
#[derive(Debug, Clone, Copy)]
struct Pacing {
    idle_poll: Duration,
    settle: Duration,
    /// How long to wait for idle.
    timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Options {
    mode: SendMode,
    send_now: bool,
}

/// Cancellation while waiting for idle, with nothing typed.
fn cancelled(pane: &str, interrupted: bool) -> HerdrError {
    HerdrError::detailed(
        "cancelled",
        format!("the client cancelled the call; nothing was typed into {pane}"),
        json!({"pane": pane, "sent": false, "interrupted": interrupted}),
    )
}

/// `agent` is the pane's agent from the snapshot the target was resolved and guarded in, so a
/// restart interrupts it without resolving and guarding the target again.
async fn send_prompt(
    client: &Client,
    pane: &str,
    agent: Option<&AgentInfo>,
    prompt: &str,
    options: Options,
    pacing: Pacing,
    ct: &CancellationToken,
) -> Result<Value, HerdrError> {
    let Options { mode, send_now } = options;
    let current = client.agent_get(pane).await?;
    let status = current.agent_status;
    let kind = current.agent.as_deref();
    if status == AgentStatus::Blocked {
        return Err(blocked(pane));
    }
    let deadline = Instant::now() + pacing.timeout;
    let (result, interrupted) = match mode {
        SendMode::Steer => {
            return steer(client, pane, prompt, status, kind, send_now, pacing.settle).await;
        }
        SendMode::Auto => match status {
            AgentStatus::Idle | AgentStatus::Done => ("started", false),
            // Decided before typing: only a kind that queues typed input gets it while working.
            AgentStatus::Working if provider(kind).queue_marker.is_some() => {
                refuse_dialog(client, pane, kind).await?;
                if ct.is_cancelled() {
                    return Err(cancelled(pane, false));
                }
                let mut out =
                    steer(client, pane, prompt, status, kind, send_now, pacing.settle).await?;
                out["mode"] = json!(SendMode::Auto);
                if out["delivery"] == "steered" {
                    // The kind queues, but its queue marker never showed: say so, never guess.
                    out["delivery"] = json!("unconfirmed");
                    let mut note = UNCONFIRMED_NOTE.to_owned();
                    if send_now {
                        note.push_str(
                            " send_now was not pressed, since no queued prompt was seen.",
                        );
                    }
                    out["note"] = json!(note);
                }
                out["result"] = out["delivery"].clone();
                return Ok(out);
            }
            AgentStatus::Working => {
                return Err(HerdrError::detailed(
                    "busy",
                    format!(
                        "{pane} is working and its kind has no input queue, so nothing was typed. Wait with herdr_wait, resend with mode after_idle to send when it is idle, or mode steer to type into the running turn"
                    ),
                    json!({"pane": pane, "status": "working", "kind": kind}),
                ));
            }
            _ => {
                return Err(HerdrError::tool(
                    "agent_status_unknown",
                    format!(
                        "{pane}'s state is not detected and nothing was sent; read the pane, or use mode steer"
                    ),
                ));
            }
        },
        SendMode::AfterIdle => {
            tokio::select! {
                idle = wait_idle(client, pacing.idle_poll, pane, deadline, false) => idle?,
                () = ct.cancelled() => return Err(cancelled(pane, false)),
            }
            ("sent_after_idle", false)
        }
        SendMode::Restart => {
            let mut interrupted = false;
            if !matches!(status, AgentStatus::Idle | AgentStatus::Done) {
                interrupt::interrupt_pane(
                    client,
                    pacing.settle,
                    pane,
                    agent,
                    interrupt::Action::default(),
                )
                .await?;
                interrupted = true;
            }
            tokio::select! {
                idle = wait_idle(client, pacing.idle_poll, pane, deadline, interrupted) => idle?,
                () = ct.cancelled() => return Err(cancelled(pane, interrupted)),
            }
            ("restarted", interrupted)
        }
    };
    if ct.is_cancelled() {
        return Err(cancelled(pane, interrupted));
    }
    let agent = gated_prompt(client, pane, prompt)
        .await
        .map_err(|err| prompt_error(err, pane, json!({"phase": "prompted", "pane": pane})))?;
    let turn = turn_for(client, &agent).await;
    let mut out = json!({
        "result": result, "mode": mode, "pane": pane,
        "status": agent.agent_status, "turn": turn, "delivery": "started",
    });
    if send_now && provider(kind).send_now_key.is_none() {
        out["note"] = json!(no_send_now_note(kind));
    }
    if mode == SendMode::Restart {
        out["interrupted"] = json!(interrupted);
    }
    Ok(out)
}

/// Waits for the turn in `out` to settle and merges the outcome in. The prompt is already
/// delivered, so nothing here fails the call: a wait that cannot finish says why, keeps the
/// turn, and adds to any earlier note.
async fn await_turn(
    client: &Client,
    inner: &Inner,
    out: &mut Value,
    remaining: Duration,
    ct: &CancellationToken,
    progress: Option<UnboundedSender<Progress>>,
) {
    let Some(turn) = serde_json::from_value::<Turn>(out["turn"].clone()).ok() else {
        add_note(
            out,
            "there is no turn to wait on (the prompt was queued, steered or unconfirmed); read the pane",
        );
        return;
    };
    if remaining.is_zero() {
        out["timed_out"] = json!(true);
        add_note(
            out,
            "the wait cap was used up before the turn could be waited on; call herdr_wait with the turn",
        );
        return;
    }
    let request = Request {
        pane: turn.pane.clone(),
        condition: Condition::Turn(turn),
        cap: remaining,
    };
    let outcome = tokio::select! {
        outcome = wait::wait(client, &inner.wait, request, progress) => outcome,
        () = ct.cancelled() => {
            out["timed_out"] = json!(true);
            return;
        }
    };
    match outcome {
        Ok(outcome) => merge_wait(out, &outcome.to_json()),
        Err(err) => wait_failed(out, &err),
    }
}

/// Merges a wait's fields into `out`, keeping its pane and adding to its note.
fn merge_wait(out: &mut Value, fields: &Value) {
    if let (Some(fields), Some(target)) = (fields.as_object(), out.as_object_mut()) {
        for (key, value) in fields {
            if key != "pane" && key != "note" {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(note) = fields["note"].as_str() {
        add_note(out, note);
    }
}

fn wait_failed(out: &mut Value, err: &HerdrError) {
    out["wait_error"] = json!({"code": err.code(), "message": err.to_string()});
    add_note(
        out,
        "the prompt was delivered but the wait failed; do not resend. Retry herdr_wait with the turn, or read the pane",
    );
}

/// Appends `text` to the note in `out`, keeping any earlier one.
fn add_note(out: &mut Value, text: &str) {
    let note = match out["note"].as_str() {
        Some(earlier) => format!("{earlier}; {text}"),
        None => text.to_owned(),
    };
    out["note"] = json!(note);
}

fn no_send_now_note(kind: Option<&str>) -> String {
    format!(
        "send_now is not supported for {}; ignored",
        kind.unwrap_or("this agent kind")
    )
}

const UNCONFIRMED_NOTE: &str = "The agent was working; its queue marker never showed, so it is not confirmed whether the prompt was queued or taken into the running turn. Read the pane before sending again.";

/// Refuses before typing when a known startup or permission dialog is on screen: typed text
/// would answer it.
async fn refuse_dialog(client: &Client, pane: &str, kind: Option<&str>) -> Result<(), HerdrError> {
    match visible_dialog(client, pane, kind.unwrap_or_default()).await {
        Some((dialog, _)) => Err(HerdrError::detailed(
            "agent_needs_input",
            format!(
                "{pane} shows a dialog ({}); nothing was typed. Read the pane, then answer it or ask the user",
                dialog.id
            ),
            json!({"pane": pane, "dialog": dialog.id}),
        )),
        None => Ok(()),
    }
}

/// Screen reads after typing into a working agent, `settle` apart, looking for its queue marker.
const QUEUE_READS: usize = 3;

const QUEUED_NOTE: &str = "The agent is mid-turn and queued this prompt; it runs when the current turn ends. Use send_now to submit it immediately, or herdr_wait on the agent to know when it finishes.";

/// Whether the visible screen shows the kind's queue marker.
async fn shows_queue_marker(client: &Client, pane: &str, marker: &str) -> bool {
    let mut params = AgentReadParams::new(pane);
    params.source = ReadSource::Visible;
    client
        .agent_read(&params)
        .await
        .is_ok_and(|read| read.text.to_lowercase().contains(marker))
}

/// Types the prompt without waiting for activity. For a working agent whose kind queues input,
/// one visible-screen read after `settle` tells a queued prompt from a steering one.
async fn steer(
    client: &Client,
    pane: &str,
    prompt: &str,
    status: AgentStatus,
    kind: Option<&str>,
    send_now: bool,
    settle: Duration,
) -> Result<Value, HerdrError> {
    client
        .agent_prompt(&AgentPromptParams::new(pane, prompt))
        .await
        .map_err(|err| prompt_error(err, pane, json!({"phase": "prompted", "pane": pane})))?;
    let provider = provider(kind);
    let working = status == AgentStatus::Working;
    let mut delivery = if working { "steered" } else { "started" };
    let mut note = None;
    if working && let Some(marker) = provider.queue_marker {
        // The marker can take a redraw or two to appear.
        let mut seen = false;
        for _ in 0..QUEUE_READS {
            sleep(settle).await;
            if shows_queue_marker(client, pane, marker).await {
                seen = true;
                break;
            }
        }
        if seen {
            delivery = "queued";
            note = Some(QUEUED_NOTE.to_string());
            if send_now && let Some(key) = provider.send_now_key {
                client
                    .agent_send_keys(pane, &[key.to_string()])
                    .await
                    .map_err(|err| {
                        HerdrError::detailed(
                            "send_now_failed",
                            format!("the prompt was typed into {pane} and queued but {key} failed: {err}"),
                            json!({"pane": pane, "prompt_sent": true, "cause": err.code()}),
                        )
                    })?;
                delivery = "sent_now";
                note = None;
            }
        }
    }
    if send_now && provider.send_now_key.is_none() {
        note = Some(no_send_now_note(kind));
    }
    let mut out = json!({
        "result": "steered", "mode": "steer", "pane": pane, "turn": null, "delivery": delivery,
    });
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

async fn wait_idle(
    client: &Client,
    idle_poll: Duration,
    pane: &str,
    deadline: Instant,
    interrupted: bool,
) -> Result<(), HerdrError> {
    loop {
        match client.agent_get(pane).await?.agent_status {
            AgentStatus::Idle | AgentStatus::Done => return Ok(()),
            AgentStatus::Blocked => return Err(blocked(pane)),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(HerdrError::detailed(
                "not_sent",
                format!("{pane} never went idle, so the prompt was not sent"),
                json!({"pane": pane, "interrupted": interrupted}),
            ));
        }
        sleep(idle_poll).await;
    }
}

/// Herdr gates the prompt on activity: it returns the `working` snapshot once the turn starts,
/// `agent_prompt_stalled` if nothing happens, and `agent_blocked` for a blocked agent.
pub(crate) async fn gated_prompt(
    client: &Client,
    target: &str,
    text: &str,
) -> Result<AgentInfo, HerdrError> {
    let mut params = AgentPromptParams::new(target, text);
    params.wait = Some(PromptWait {
        until: vec![AgentStatus::Working, AgentStatus::Blocked],
        timeout_ms: Some(ACTIVITY_GATE_MS),
    });
    match client.agent_prompt(&params).await? {
        Outcome::Done(agent) => Ok(agent),
        Outcome::TimedOut { message } => Err(HerdrError::Api {
            code: ApiErrorCode::AgentPromptStalled,
            message,
        }),
    }
}

/// The turn token for the agent snapshot a prompt returned, or `None` when it is neither working
/// nor blocked.
pub(crate) async fn turn_for(client: &Client, agent: &AgentInfo) -> Option<Turn> {
    let shell = client
        .pane_process_info(&agent.pane_id)
        .await
        .ok()
        .and_then(|info| info.shell_pid);
    Turn::from_agent(agent, shell)
}

pub(crate) fn prompt_error(err: HerdrError, pane: &str, details: Value) -> HerdrError {
    match err {
        HerdrError::Api {
            code: ApiErrorCode::AgentPromptStalled,
            ..
        } => HerdrError::detailed(
            "agent_prompt_stalled",
            format!(
                "{pane} showed no activity after the prompt; it may or may not have arrived; read the pane before sending again"
            ),
            details,
        ),
        HerdrError::Api {
            code: ApiErrorCode::AgentBlocked,
            ..
        } => HerdrError::detailed(
            "agent_blocked",
            format!("{pane} is blocked on a prompt; read the pane before doing anything else"),
            details,
        ),
        other => other,
    }
}

async fn send_raw(
    client: &Client,
    resolution: &Resolution,
    pane: &str,
    is_agent: bool,
    params: &SendParams,
) -> Result<Value, HerdrError> {
    let answering = params.answer_blocked == Some(true);
    let agent = if is_agent {
        Some(client.agent_get(pane).await?)
    } else {
        None
    };
    let is_blocked = agent
        .as_ref()
        .is_some_and(|agent| agent.agent_status == AgentStatus::Blocked);
    let refused =
        |why: &str| HerdrError::tool("answer_refused", format!("nothing was typed: {why}"));
    if answering {
        let Some(agent) = &agent else {
            return Err(HerdrError::tool(
                "not_an_agent",
                format!("{pane} has no agent to answer"),
            ));
        };
        let caller = resolution.caller.as_ref().map(|c| c.pane_id.as_str());
        let own_child = resolution.caller_verified
            && caller.is_some()
            && provenance::answer_parent_of(&agent.tokens) == caller;
        if !own_child {
            return Err(refused(
                "only a verified caller may answer an agent it launched itself",
            ));
        }
        // Herdr reports some startup dialogs as idle, so a known dialog on screen counts too.
        if !is_blocked
            && visible_dialog(client, pane, agent.agent.as_deref().unwrap_or_default())
                .await
                .is_none()
        {
            return Err(HerdrError::tool(
                "not_blocked",
                format!("{pane} is not blocked, so there is nothing to answer; nothing was typed"),
            ));
        }
    } else if is_blocked {
        return Err(refused(
            "the agent is blocked on a prompt; pass answer_blocked to answer an agent you launched",
        ));
    }

    match (&params.text, &params.keys) {
        // One request: Herdr validates the keys before it types any of it.
        (Some(text), Some(keys)) if !is_agent => {
            client
                .pane_send_input(&PaneSendInputParams {
                    pane_id: pane.to_string(),
                    text: text.clone(),
                    keys: keys.clone(),
                })
                .await?;
        }
        (text, keys) => {
            if let Some(text) = text {
                client.pane_send_text(pane, text).await?;
            }
            if let Some(keys) = keys {
                let sent = if is_agent {
                    client.agent_send_keys(pane, keys).await
                } else {
                    client.pane_send_keys(pane, keys).await
                };
                if let Err(err) = sent {
                    return Err(if text.is_some() {
                        HerdrError::detailed(
                            "keys_failed",
                            format!("the text was typed into {pane} but the keys failed: {err}"),
                            json!({"pane": pane, "text_sent": true, "cause": err.code()}),
                        )
                    } else {
                        err
                    });
                }
            }
        }
    }
    Ok(json!({
        "result": if answering { "answered" } else { "sent" },
        "pane": pane,
        "text_sent": params.text.is_some(),
        "keys": params.keys,
    }))
}
