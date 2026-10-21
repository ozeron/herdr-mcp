use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::caller::Resolution;
use crate::guard::TargetKind;
use crate::herdr::{
    AgentInfo, AgentPromptParams, AgentStatus, ApiErrorCode, Client, HerdrError, Outcome,
    PaneSendInputParams, PromptWait,
};
use crate::provenance;
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;
use crate::tools::interrupt;
use crate::tools::launch::visible_dialog;
use crate::tools::{DEFAULT_TIMEOUT, clamp_timeout};
use crate::turn::Turn;

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
    /// For a prompt. auto (default) refuses a working agent with busy; after_idle waits for idle, then sends; steer types into a working agent and returns no turn; restart interrupts, waits for idle, then sends.
    pub mode: Option<SendMode>,
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
    params: SendParams,
) -> Result<Value, HerdrError> {
    validate(&params)?;
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
    let pane = scoped
        .target
        .as_ref()
        .expect("target is required")
        .id
        .as_str();
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
            send_prompt(client, pane, agent, prompt, mode, pacing, ct).await
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
    mode: SendMode,
    pacing: Pacing,
    ct: &CancellationToken,
) -> Result<Value, HerdrError> {
    let status = client.agent_get(pane).await?.agent_status;
    if status == AgentStatus::Blocked {
        return Err(blocked(pane));
    }
    let deadline = Instant::now() + pacing.timeout;
    let (result, interrupted) = match mode {
        SendMode::Steer => return steer(client, pane, prompt).await,
        SendMode::Auto => match status {
            AgentStatus::Idle | AgentStatus::Done => ("started", false),
            AgentStatus::Working => {
                return Err(HerdrError::detailed(
                    "busy",
                    format!(
                        "{pane} is working and nothing was sent; use mode after_idle to queue behind it, steer to type into it, or restart"
                    ),
                    json!({"pane": pane, "status": "working"}),
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
        "status": agent.agent_status, "turn": turn,
    });
    if mode == SendMode::Restart {
        out["interrupted"] = json!(interrupted);
    }
    Ok(out)
}

async fn steer(client: &Client, pane: &str, prompt: &str) -> Result<Value, HerdrError> {
    client
        .agent_prompt(&AgentPromptParams::new(pane, prompt))
        .await
        .map_err(|err| prompt_error(err, pane, json!({"phase": "prompted", "pane": pane})))?;
    Ok(json!({"result": "steered", "mode": "steer", "pane": pane, "turn": null}))
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
