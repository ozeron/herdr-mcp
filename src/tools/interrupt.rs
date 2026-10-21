use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::guard::TargetKind;
use crate::herdr::{AgentInfo, AgentStatus, Client, HerdrError, PaneProcessInfo};
use crate::providers::{PROCESS_INTERRUPT_KEY, provider};
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;

pub const SETTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct InterruptParams {
    /// Pane ID or agent name.
    pub target: String,
    /// auto (default) picks by status; keys sends keys; signal sends INT or TERM to the foreground process group.
    pub method: Option<Method>,
    /// Keys for method keys. Defaults to the agent kind's interrupt key, or ctrl+c for a process pane.
    pub keys: Option<Vec<String>>,
    /// Signal for method signal. Defaults to INT.
    pub signal: Option<Signal>,
    /// Allow interrupting your own pane. Only when the user asked for it.
    pub allow_self: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Auto,
    Keys,
    Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "UPPERCASE")]
pub enum Signal {
    Int,
    Term,
}

impl Signal {
    fn number(self) -> i32 {
        match self {
            Self::Int => libc::SIGINT,
            Self::Term => libc::SIGTERM,
        }
    }
}

fn label(info: &PaneProcessInfo) -> &'static str {
    match info.interrupt_group() {
        None => "shell",
        Some(_) => "running",
    }
}

fn no_active_work(pane: &str, method: &str, status_after: impl Serialize) -> Value {
    json!({"result": "no_active_work", "method": method, "pane": pane, "status_after": status_after})
}

pub async fn run(inner: &Inner, params: InterruptParams) -> Result<Value, HerdrError> {
    let mut scope = Scope::open(inner)?;
    let request = TargetRequest::new(
        &params.target,
        &[TargetKind::Pane],
        "herdr_interrupt takes a pane ID or an agent name",
    )
    .allow_self(params.allow_self.unwrap_or(false));
    let scoped = scope.target(request).await?;
    let pane = scoped.required().id.as_str();
    let agent = scoped.agent();
    let action = Action {
        method: params.method,
        keys: params.keys,
        signal: params.signal,
    };
    interrupt_pane(scope.client(), inner.interrupt_settle, pane, agent, action).await
}

/// How to interrupt, apart from where.
#[derive(Debug, Default)]
pub(crate) struct Action {
    pub method: Option<Method>,
    pub keys: Option<Vec<String>>,
    pub signal: Option<Signal>,
}

/// Interrupts a pane the caller has already resolved and guarded. `agent` is the pane's agent
/// from the snapshot the target was resolved in, if it has one.
pub(crate) async fn interrupt_pane(
    client: &Client,
    settle: Duration,
    pane: &str,
    agent: Option<&AgentInfo>,
    action: Action,
) -> Result<Value, HerdrError> {
    let agent_provider = agent.map(|agent| provider(agent.agent.as_deref()));

    match action.method.unwrap_or(Method::Auto) {
        Method::Signal => signal(client, settle, pane, action.signal.unwrap_or(Signal::Int)).await,
        Method::Keys => {
            let default = agent_provider.map_or(PROCESS_INTERRUPT_KEY, |p| p.interrupt_key);
            let keys = action.keys.unwrap_or_else(|| vec![default.to_string()]);
            // Anything but the interrupt key would answer the prompt, which only a launcher may do.
            if agent_provider.is_some()
                && client.agent_get(pane).await?.agent_status == AgentStatus::Blocked
                && !keys.iter().all(|key| key.eq_ignore_ascii_case(default))
            {
                return Err(HerdrError::tool(
                    "answer_refused",
                    format!(
                        "nothing was typed: the agent is blocked on a prompt, so only {default} (which denies it) is allowed here; use herdr_send with answer_blocked to answer"
                    ),
                ));
            }
            send_keys(client, settle, pane, agent_provider.is_some(), keys, None).await
        }
        Method::Auto => match agent_provider {
            Some(provider) => {
                let status = client.agent_get(pane).await?.agent_status;
                match status {
                    AgentStatus::Working => {
                        let keys = vec![provider.interrupt_key.to_string()];
                        send_keys(client, settle, pane, true, keys, provider.interrupt_note).await
                    }
                    AgentStatus::Idle | AgentStatus::Done => {
                        Ok(no_active_work(pane, "auto", status))
                    }
                    AgentStatus::Blocked => Err(HerdrError::tool(
                        "agent_blocked",
                        "the agent is blocked on a prompt and Esc would deny it; answer it, or pass method keys or signal to interrupt anyway",
                    )),
                    AgentStatus::Unknown => Err(HerdrError::tool(
                        "agent_status_unknown",
                        "the agent's state is not detected; pass method keys or signal to interrupt anyway",
                    )),
                }
            }
            None => {
                let info = client.pane_process_info(pane).await?;
                if info.interrupt_group().is_none() {
                    return Ok(no_active_work(pane, "auto", "shell"));
                }
                let keys = vec![PROCESS_INTERRUPT_KEY.to_string()];
                send_keys(client, settle, pane, false, keys, None).await
            }
        },
    }
}

async fn status_after(
    client: &Client,
    settle: Duration,
    pane: &str,
    is_agent: bool,
) -> Result<Value, HerdrError> {
    tokio::time::sleep(settle).await;
    if is_agent {
        Ok(json!(client.agent_get(pane).await?.agent_status))
    } else {
        Ok(json!(label(&client.pane_process_info(pane).await?)))
    }
}

async fn send_keys(
    client: &Client,
    settle: Duration,
    pane: &str,
    is_agent: bool,
    keys: Vec<String>,
    note: Option<&str>,
) -> Result<Value, HerdrError> {
    if is_agent {
        client.agent_send_keys(pane, &keys).await?;
    } else {
        client.pane_send_keys(pane, &keys).await?;
    }
    let mut out = json!({
        "result": "interrupt_requested", "method": "keys", "pane": pane, "keys": keys,
        "status_after": status_after(client, settle, pane, is_agent).await?,
    });
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

async fn signal(
    client: &Client,
    settle: Duration,
    pane: &str,
    signal: Signal,
) -> Result<Value, HerdrError> {
    let info = client.pane_process_info(pane).await?;
    let Some(pgid) = info.interrupt_group() else {
        return Ok(no_active_work(pane, "signal", "shell"));
    };
    let Some(group) = i32::try_from(pgid).ok().filter(|group| *group > 1) else {
        return Err(HerdrError::tool(
            "invalid_pgid",
            format!("refusing to signal process group {pgid}"),
        ));
    };
    // SAFETY: kill takes plain integers and touches no memory.
    let rc = unsafe { libc::kill(-group, signal.number()) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return match errno.raw_os_error() {
            Some(libc::ESRCH) => Ok(no_active_work(pane, "signal", "exited")),
            Some(libc::EPERM) => {
                let names: Vec<&str> = info
                    .foreground_processes
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect();
                Err(HerdrError::tool(
                    "signal_not_permitted",
                    format!(
                        "not permitted to signal process group {pgid} ({})",
                        names.join(", ")
                    ),
                ))
            }
            _ => Err(HerdrError::tool(
                "signal_failed",
                format!("signalling process group {pgid} failed: {errno}"),
            )),
        };
    }
    let out = json!({
        "result": "interrupt_requested", "method": "signal", "pane": pane,
        "signal": signal, "pgid": pgid,
        "status_after": status_after(client, settle, pane, false).await?,
    });
    Ok(out)
}
