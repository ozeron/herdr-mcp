use rmcp::RoleServer;
use rmcp::schemars::JsonSchema;
use rmcp::service::RequestContext;
use serde::Deserialize;
use serde_json::Value;

use crate::guard::TargetKind;
use crate::herdr::{AgentStatus, HerdrError};
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;
use crate::tools::DEFAULT_TIMEOUT;
use crate::turn::Turn;
use crate::wait::{self, Condition, Request};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct OutputSpec {
    /// Text to look for in output added after the wait began.
    #[serde(rename = "match")]
    pub pattern: String,
    /// A command already typed into the pane, as sent. Only output after its last echoed line can match, so a line printed before this wait began still matches. Use the `rewait` that `herdr_run` returns.
    pub after_command: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ProcessExitSpec {
    /// Process group from `herdr_run`. Without it, the first non-shell foreground group is tracked.
    pub pgid: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WaitParams {
    /// Pane ID or agent name. Not needed with `turn`.
    pub target: Option<String>,
    /// A turn token returned by `herdr_launch` or `herdr_send`, passed back unchanged. Waits for that turn to settle.
    pub turn: Option<Turn>,
    /// Wait until the agent has one of these statuses.
    pub until: Option<Vec<AgentStatus>>,
    /// Wait for text to appear in the pane.
    pub output: Option<OutputSpec>,
    /// Wait for the pane's foreground process group to finish. No exit code is available.
    pub process_exit: Option<ProcessExitSpec>,
    /// How long to wait, in milliseconds. Default 30000, capped at the wait cap.
    pub timeout_ms: Option<u64>,
    /// Allow waiting on your own pane. Only when the user asked for it.
    pub allow_self: Option<bool>,
}

fn condition(params: &WaitParams) -> Result<Condition, HerdrError> {
    let given = [
        params.turn.is_some(),
        params.until.is_some(),
        params.output.is_some(),
        params.process_exit.is_some(),
    ]
    .into_iter()
    .filter(|set| *set)
    .count();
    if given != 1 {
        return Err(HerdrError::invalid_params(
            "give exactly one of turn, until, output or process_exit",
        ));
    }
    if let Some(turn) = &params.turn {
        return Ok(Condition::Turn(turn.clone()));
    }
    if let Some(until) = &params.until {
        if until.is_empty() {
            return Err(HerdrError::invalid_params(
                "until must list at least one status",
            ));
        }
        return Ok(Condition::Until(until.clone()));
    }
    if let Some(output) = &params.output {
        if output.pattern.is_empty() {
            return Err(HerdrError::invalid_params("output.match must not be empty"));
        }
        return Ok(Condition::Output {
            needle: output.pattern.clone(),
            after_command: output
                .after_command
                .clone()
                .filter(|c| !c.trim().is_empty()),
        });
    }
    let pgid = params.process_exit.as_ref().and_then(|spec| spec.pgid);
    Ok(Condition::ProcessExit { pgid })
}

pub async fn run(
    inner: &Inner,
    ctx: &RequestContext<RoleServer>,
    params: WaitParams,
) -> Result<Value, HerdrError> {
    let condition = condition(&params)?;
    if params.turn.is_none() && params.target.is_none() {
        return Err(HerdrError::invalid_params(
            "target is required unless a turn token is given",
        ));
    }
    let client_info = ctx.peer.peer_info();
    let client_name = client_info
        .as_ref()
        .map(|info| info.client_info.name.as_str());
    let cap = inner.profile.wait_cap(client_name);
    let (timeout, note) = super::clamp_timeout(params.timeout_ms, cap, DEFAULT_TIMEOUT);

    let mut scope = Scope::open(inner)?;
    let allow_self = params.allow_self.unwrap_or(false);
    let pane = match &params.turn {
        Some(turn) => {
            scope
                .guard_pane(&turn.pane, allow_self, params.target.as_deref())
                .await?;
            turn.pane.clone()
        }
        None => {
            let target = params.target.as_deref().unwrap_or_default();
            let request = TargetRequest::new(
                target,
                &[TargetKind::Pane],
                "herdr_wait waits on a pane or an agent, not a tab or workspace",
            )
            .allow_self(allow_self)
            .self_guard_first();
            let scoped = scope.target(request).await?;
            scoped.target.expect("target is required").id
        }
    };

    let request = Request {
        pane,
        condition,
        cap: timeout,
    };
    let work = wait::wait(
        scope.client(),
        &inner.wait,
        request,
        super::progress_sink(ctx),
    );
    tokio::pin!(work);
    let mut out = tokio::select! {
        out = &mut work => out?,
        () = ctx.ct.cancelled() => {
            return Err(HerdrError::tool("cancelled", "the client cancelled the wait"));
        }
    };
    if let Some(note) = note {
        out["note"] = match out["note"].as_str() {
            Some(existing) => serde_json::json!(format!("{existing}; {note}")),
            None => serde_json::json!(note),
        };
    }
    Ok(out)
}
