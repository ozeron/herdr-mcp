use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::caller::resolve_caller;
use crate::cli::ToolMode;
use crate::herdr::{AgentInfo, AgentStartParams, AgentStatus, ApiErrorCode, Client, HerdrError};
use crate::install::shim::SHIM_NAME;
use crate::matcher::{read_text, tail};
use crate::place::{self, Options, Placed, Where};
use crate::providers::{is_ready, mcp_args};
use crate::server::Inner;
use crate::tools::DEFAULT_TIMEOUT;
use crate::tools::capabilities::{McpStatus, mcp_status};
use crate::tools::send::{gated_prompt, prompt_error, turn_for};
use crate::turn::TurnFrom;
use crate::wait::{self, Condition, Progress, Request};

pub const START_RETRY_INTERVAL: Duration = Duration::from_millis(100);
pub const START_RETRY_WINDOW: Duration = Duration::from_secs(2);
pub const READY_POLL: Duration = Duration::from_millis(250);
pub const QUIET_WINDOW: Duration = Duration::from_secs(3);
pub const STARTUP_GRACE: Duration = Duration::from_secs(3);
pub const IDLE_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy)]
pub struct LaunchTuning {
    /// Pause between `agent.start` attempts while a fresh pane reports busy.
    pub start_retry_interval: Duration,
    /// How long a fresh pane may keep reporting busy.
    pub start_retry_window: Duration,
    /// Poll while waiting for the agent to become ready.
    pub ready_poll: Duration,
    /// How long the agent must show no state change after first ready.
    pub quiet_window: Duration,
    /// Poll while `herdr_send` waits for an idle agent.
    pub idle_poll: Duration,
    /// How long the shell may stay in front after `agent.start` before the launch counts as exited.
    pub startup_grace: Duration,
}

impl Default for LaunchTuning {
    fn default() -> Self {
        Self {
            start_retry_interval: START_RETRY_INTERVAL,
            start_retry_window: START_RETRY_WINDOW,
            ready_poll: READY_POLL,
            quiet_window: QUIET_WINDOW,
            idle_poll: IDLE_POLL,
            startup_grace: STARTUP_GRACE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum LaunchWait {
    /// Wait for the first turn to settle, bounded by the wait cap.
    Turn,
    /// Return once the prompt has started a turn.
    None,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct LaunchParams {
    /// Agent kind: claude or codex.
    pub kind: String,
    /// Unique agent name. Refused while an agent with this name is live.
    pub name: String,
    /// The first prompt. Without it the agent is started and left idle.
    pub instructions: Option<String>,
    /// Where to start it. Default: a new tab in the calling pane's workspace.
    #[serde(rename = "where")]
    pub placement: Option<Where>,
    /// Working directory for a new pane.
    pub cwd: Option<String>,
    /// Environment for a new pane.
    pub env: Option<HashMap<String, String>>,
    /// Name for a new tab, workspace or worktree.
    pub label: Option<String>,
    /// Give the child control tools. Default: read tools only.
    pub control: Option<bool>,
    /// turn waits for the first turn to settle; none (default) returns the turn token.
    pub wait: Option<LaunchWait>,
    /// How long the agent may take to become ready, in milliseconds. Default 30000.
    pub start_timeout_ms: Option<u64>,
}

fn invalid(message: &str) -> HerdrError {
    HerdrError::tool("invalid_params", message)
}

/// What this call made, reported with every phase so a timeout or cancel never leads to a repeat.
fn progress(placed: &Placed, name: &str, phase: &str) -> Value {
    json!({
        "phase": phase, "name": name, "pane": placed.pane_id, "tab": placed.tab_id,
        "workspace": placed.workspace_id, "created": placed.created_json(),
    })
}

fn detailed(err: HerdrError, details: Value) -> HerdrError {
    match err {
        HerdrError::Tool { code, message } => HerdrError::detailed(code, message, details),
        other => other,
    }
}

pub async fn run(
    inner: &Inner,
    client_info_name: Option<&str>,
    cancel: CancellationToken,
    ticks: Option<UnboundedSender<Progress>>,
    params: LaunchParams,
) -> Result<Value, HerdrError> {
    if params.name.trim().is_empty() || params.kind.trim().is_empty() {
        return Err(invalid("kind and name must not be empty"));
    }
    let wait = params.wait.unwrap_or(LaunchWait::None);
    match params.instructions.as_deref() {
        Some("") => return Err(invalid("instructions must not be empty")),
        None if wait == LaunchWait::Turn => {
            return Err(invalid("wait turn needs instructions to start a turn"));
        }
        _ => {}
    }
    let control = params.control.unwrap_or(false);
    if control && inner.mode != ToolMode::Control {
        return Err(HerdrError::tool(
            "control_refused",
            "this bridge is in read mode, so it cannot hand control tools to a child",
        ));
    }
    let options = Options {
        cwd: params.cwd,
        env: params.env.unwrap_or_default(),
        label: params.label,
    };
    place::validate(params.placement.as_ref(), &options)?;

    let client = inner.client()?;
    let (snapshot, resolution) = tokio::try_join!(
        client.session_snapshot(),
        resolve_caller(&client, &inner.caller_env, inner.ancestry.as_ref()),
    )?;
    if snapshot
        .agents
        .iter()
        .any(|agent| agent.name.as_deref() == Some(params.name.as_str()))
    {
        return Err(HerdrError::tool(
            "name_taken",
            format!(
                "an agent named {:?} is live; pick another name",
                params.name
            ),
        ));
    }
    let placed = place::place(&client, &resolution, params.placement.as_ref(), &options).await?;

    let (args, mcp) = match injection(inner, &params.kind, control) {
        Ok(found) => found,
        Err(err) => {
            placed.cleanup(&client).await;
            return Err(err);
        }
    };
    let mut start = AgentStartParams::new(&params.name, &params.kind, &placed.pane_id);
    start.args = args;
    if let Err(err) = start_agent(&client, &inner.launch, &placed.pane_id, &start).await {
        placed.cleanup(&client).await;
        return Err(err);
    }

    let timeout = params
        .start_timeout_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let ready = await_ready(
        &client,
        &inner.launch,
        &placed,
        &params.kind,
        &params.name,
        timeout,
    )
    .await;
    if let Err(failure) = ready {
        if failure.cleanup {
            placed.cleanup(&client).await;
        }
        return Err(failure.err);
    }

    let mut out = progress(&placed, &params.name, "started");
    out["kind"] = json!(params.kind);
    out["mcp"] = json!(mcp);
    out["turn"] = Value::Null;
    let Some(instructions) = params.instructions.as_deref() else {
        return Ok(out);
    };

    let agent = gated_prompt(&client, &placed.pane_id, instructions)
        .await
        .map_err(|err| {
            let phase = if matches!(
                err,
                HerdrError::Api {
                    code: ApiErrorCode::AgentBlocked,
                    ..
                }
            ) {
                "started"
            } else {
                "prompted"
            };
            prompt_error(err, &placed.pane_id, progress(&placed, &params.name, phase))
        })?;
    let turn = turn_for(&client, &agent).await;
    out["phase"] = json!("prompted");
    out["status"] = json!(agent.agent_status);
    out["turn"] = json!(turn);

    let Some(turn) = turn else {
        out["note"] = json!(
            "the prompt was accepted but the agent is neither working nor blocked, so there is no turn to wait on; read the pane"
        );
        return Ok(out);
    };
    if wait != LaunchWait::Turn {
        return Ok(out);
    }
    out["phase"] = json!("waiting");
    if turn.from == TurnFrom::Blocked {
        out["matched"] = json!(true);
        out["timed_out"] = json!(false);
        out["state_change_seq"] = json!(agent.state_change_seq);
        out["completion_seq"] = json!(agent.completion_seq);
        out["excerpt"] = json!(
            read_text(&client, &placed.pane_id)
                .await
                .map(|text| tail(&text))
                .unwrap_or_default()
        );
        return Ok(out);
    }

    let cap = inner.profile.wait_cap(client_info_name);
    let waiting = progress(&placed, &params.name, "waiting");
    let result = tokio::select! {
        result = wait::wait(
            &client,
            &inner.wait,
            Request { pane: turn.pane.clone(), condition: Condition::Turn(turn.clone()), cap },
            ticks,
        ) => result,
        () = cancel.cancelled() => {
            let mut details = waiting.clone();
            details["turn"] = json!(turn);
            return Err(HerdrError::detailed(
                "cancelled",
                "the client cancelled the wait; the agent keeps working. Call herdr_wait with the turn to resume",
                details,
            ));
        }
    };
    let mut details = waiting;
    details["turn"] = json!(turn);
    match result {
        Ok(Value::Object(fields)) => {
            for (key, value) in fields {
                out[key] = value;
            }
            Ok(out)
        }
        Ok(other) => Ok(other),
        Err(err) => Err(detailed(err, details)),
    }
}

/// Per-launch MCP args apply only when no user-scope entry of ours exists, and carry
/// `--tools read` unless the call asked for control.
fn injection(
    inner: &Inner,
    kind: &str,
    control: bool,
) -> Result<(Vec<String>, &'static str), HerdrError> {
    let status = mcp_status(inner, kind);
    if status != McpStatus::Injectable {
        return Ok((Vec::new(), status.as_str()));
    }
    let mode = if control {
        ToolMode::Control
    } else {
        ToolMode::Read
    };
    let bin = bridge_binary(inner)?;
    match mcp_args(kind, &bin, mode) {
        Some(args) => Ok((args, McpStatus::Injected.as_str())),
        None => Ok((Vec::new(), McpStatus::Unsupported.as_str())),
    }
}

fn bridge_binary(inner: &Inner) -> Result<String, HerdrError> {
    let shim: Option<PathBuf> = match &inner.install_env {
        Some(env) => Some(env.shim_path()),
        None => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/bin").join(SHIM_NAME)),
    };
    if let Some(shim) = shim.filter(|shim| shim.exists()) {
        return Ok(shim.display().to_string());
    }
    let exe = match &inner.install_env {
        Some(env) => Ok(env.exe.clone()),
        None => std::env::current_exe(),
    };
    exe.map(|exe| exe.display().to_string()).map_err(|err| {
        HerdrError::tool(
            "bridge_binary_unknown",
            format!("cannot locate this binary to hand to the child: {err}"),
        )
    })
}

async fn terminal_of(client: &Client, pane: &str) -> Option<String> {
    let snapshot = client.session_snapshot().await.ok()?;
    snapshot
        .panes
        .into_iter()
        .find(|candidate| candidate.pane_id == pane)
        .map(|candidate| candidate.terminal_id)
}

/// A fresh pane reports `agent_pane_busy` while its shell initialises. Retry while
/// the shell is in front and the terminal is unchanged.
async fn start_agent(
    client: &Client,
    tuning: &LaunchTuning,
    pane: &str,
    params: &AgentStartParams,
) -> Result<(), HerdrError> {
    let deadline = Instant::now() + tuning.start_retry_window;
    let mut terminal: Option<String> = None;
    loop {
        let busy = match client.agent_start(params).await {
            Ok(_) => return Ok(()),
            Err(
                err @ HerdrError::Api {
                    code: ApiErrorCode::AgentPaneBusy,
                    ..
                },
            ) => err,
            Err(err) => return Err(err),
        };
        if Instant::now() >= deadline {
            return Err(busy);
        }
        let (info, current) =
            tokio::join!(client.pane_process_info(pane), terminal_of(client, pane));
        let initialising = info.is_ok_and(|info| info.shell_in_front_strict());
        let unchanged = match (&terminal, &current) {
            (Some(was), Some(now)) => was == now,
            (None, Some(_)) => true,
            _ => false,
        };
        if !initialising || !unchanged {
            return Err(busy);
        }
        terminal = current;
        sleep(tuning.start_retry_interval).await;
    }
}

struct NotReady {
    err: HerdrError,
    cleanup: bool,
}

/// Readiness plus the probed quiet window: after first ready, the agent must show no
/// `state_change_seq` change for `quiet_window` before it is prompted.
async fn await_ready(
    client: &Client,
    tuning: &LaunchTuning,
    placed: &Placed,
    kind: &str,
    name: &str,
    timeout: Duration,
) -> Result<AgentInfo, NotReady> {
    let deadline = Instant::now() + timeout;
    let pane = placed.pane_id.as_str();
    let mut seen_ready = false;
    let mut last_seq = 0;
    let mut quiet_since = Instant::now();
    let started = quiet_since;
    let mut seen_group = false;
    loop {
        let agent = client
            .agent_get(pane)
            .await
            .map_err(|err| NotReady { err, cleanup: true })?;
        if agent.agent_status == AgentStatus::Blocked {
            return Err(NotReady {
                err: HerdrError::detailed(
                    "agent_not_ready",
                    format!(
                        "{} is blocked during startup, probably on a trust or login prompt. The pane is kept: read it, answer it, then continue with herdr_send",
                        name
                    ),
                    progress(placed, name, "started"),
                ),
                cleanup: false,
            });
        }
        if agent.launch_pending
            && let Ok(info) = client.pane_process_info(pane).await
        {
            // A non-shell foreground group means the typed command has exec'd.
            if info.left_shell_group().is_some() {
                seen_group = true;
            } else if info.shell_in_front_strict()
                && (seen_group || started.elapsed() >= tuning.startup_grace)
            {
                let mut details = progress(placed, name, "started");
                details["cleaned_up"] = json!(placed.created.is_some());
                return Err(NotReady {
                    err: HerdrError::detailed(
                        "agent_exited",
                        format!(
                            "{kind} exited before it was ready; check the agent's arguments and login"
                        ),
                        details,
                    ),
                    cleanup: true,
                });
            }
        }

        let now = Instant::now();
        if seen_ready && agent.state_change_seq != last_seq {
            quiet_since = now;
        }
        last_seq = agent.state_change_seq;
        if is_ready(Some(kind), &agent) {
            if !seen_ready {
                seen_ready = true;
                quiet_since = now;
            }
            if now.duration_since(quiet_since) >= tuning.quiet_window {
                return Ok(agent);
            }
        }

        if now >= deadline {
            // One agent.get after a readiness timeout releases the name Herdr reserved for the start.
            let _ = client.agent_get(pane).await;
            return Err(NotReady {
                err: HerdrError::detailed(
                    "agent_not_ready",
                    format!(
                        "{} was not ready after {} ms. The pane is kept: read it, then continue with herdr_send or close it",
                        name,
                        timeout.as_millis()
                    ),
                    progress(placed, name, "started"),
                ),
                cleanup: false,
            });
        }
        sleep(
            tuning
                .ready_poll
                .min(deadline.saturating_duration_since(now)),
        )
        .await;
    }
}
