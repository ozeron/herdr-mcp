use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caller::resolve_caller;
use crate::herdr::{
    AgentReadParams, ApiErrorCode, Client, HerdrError, PaneReadParams, PaneReadResult, ReadSource,
};
use crate::output::{last_meaningful, strip_ansi, truncate_tail};
use crate::server::Inner;

pub const DEFAULT_LINES: u32 = 80;
pub const MAX_LINES: u32 = 1000;
pub const DEFAULT_MAX_CHARS: usize = 32_000;
pub const MAX_CHARS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Source {
    Recent,
    Visible,
}

impl Source {
    fn herdr(self) -> ReadSource {
        match self {
            Self::Recent => ReadSource::Recent,
            Self::Visible => ReadSource::Visible,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadParams {
    /// Pane ID or agent name. Defaults to the calling pane.
    pub target: Option<String>,
    /// `recent` is scrollback; `visible` is the current screen. Default `recent`.
    pub source: Option<Source>,
    /// Lines to return. Blank lines and box-drawing rules are skipped unless `raw`. Default 80, capped at 1000.
    pub lines: Option<u32>,
    /// Character cap on the returned text; the oldest output is cut first. Default 32000, capped at 100000.
    pub max_chars: Option<usize>,
    /// Return the terminal rows as they are, blank lines and UI rules included. Default false.
    pub raw: Option<bool>,
}

pub async fn run(inner: &Inner, params: ReadParams) -> Result<Value, HerdrError> {
    let client = inner.client()?;
    let target = match params.target {
        Some(target) => target,
        None => resolve_caller(&client, &inner.caller_env, inner.ancestry.as_ref())
            .await?
            .caller
            .map(|caller| caller.pane_id)
            .ok_or_else(|| {
                HerdrError::tool("no_target", "no target given and no caller pane is known")
            })?,
    };

    let requested = params.lines.unwrap_or(DEFAULT_LINES).max(1);
    let lines = requested.min(MAX_LINES);
    let max_chars = params
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, MAX_CHARS);
    let mut source = params.source.unwrap_or(Source::Recent);
    let mut notes = Vec::new();
    if requested > lines {
        notes.push(format!("lines capped at {MAX_LINES}"));
    }
    let raw = params.raw.unwrap_or(false);
    let rows = if raw {
        lines
    } else {
        lines.saturating_mul(4).min(MAX_LINES)
    };

    let (first, agent) = tokio::join!(read_agent(&client, &target, source, rows), async {
        match inner.client() {
            Ok(client) => client.agent_get(&target).await.ok(),
            Err(_) => None,
        }
    });
    let status = agent.map(|agent| agent.agent_status);
    let result = match first {
        Err(HerdrError::Api {
            code: ApiErrorCode::AgentNotIdle,
            ..
        }) if source == Source::Recent => {
            source = Source::Visible;
            notes.push(
                "agent is busy in a full-screen UI, so scrollback is unavailable; returned the visible screen"
                    .to_string(),
            );
            read_agent(&client, &target, source, rows).await?
        }
        Err(HerdrError::Api {
            code: ApiErrorCode::AgentNotFound,
            ..
        }) => read_pane(&client, &target, source, rows).await?,
        other => other?,
    };

    let clean = strip_ansi(&result.text);
    let clean = if raw {
        clean
    } else {
        last_meaningful(&clean, lines as usize)
    };
    let (text, cut) = truncate_tail(clean, max_chars);
    if cut {
        notes.push(format!("text cut to the last {max_chars} characters"));
    }
    let truncated = result.truncated || cut || requested > lines;
    let mut out = json!({
        "pane_id": result.pane_id,
        "source": source.herdr(),
        "lines": lines,
        "text": text,
        "truncated": truncated,
    });
    if let Some(status) = status {
        out["status"] = json!(status);
    }
    if !notes.is_empty() {
        out["note"] = Value::String(notes.join("; "));
    }
    Ok(out)
}

async fn read_agent(
    client: &Client,
    target: &str,
    source: Source,
    lines: u32,
) -> Result<PaneReadResult, HerdrError> {
    let mut params = AgentReadParams::new(target);
    params.source = source.herdr();
    params.lines = Some(lines);
    client.agent_read(&params).await
}

async fn read_pane(
    client: &Client,
    target: &str,
    source: Source,
    lines: u32,
) -> Result<PaneReadResult, HerdrError> {
    let mut params = PaneReadParams::new(target);
    params.source = source.herdr();
    params.lines = Some(lines);
    match client.pane_read(&params).await {
        Err(HerdrError::Api {
            code: ApiErrorCode::PaneNotFound | ApiErrorCode::InvalidRequest,
            ..
        }) => Err(HerdrError::tool(
            "target_not_found",
            format!("no pane or agent matches {target:?}"),
        )),
        other => other,
    }
}
