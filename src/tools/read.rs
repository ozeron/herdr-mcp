use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herdr::{
    AgentReadParams, ApiErrorCode, Client, HerdrError, PaneReadParams, PaneReadResult, ReadSource,
};
use crate::output::{is_meaningful, last_meaningful, strip_ansi, truncate_tail};
use crate::server::Inner;

pub const DEFAULT_LINES: u32 = 120;
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
    /// `recent` (default) is the latest output, scrollback included; `visible` is exactly what is on screen now.
    pub source: Option<Source>,
    /// Lines to return; blank lines and box-drawing rules are skipped. Default 120, enough for an agent's latest turn: leave it unset. Capped at 1000.
    pub lines: Option<u32>,
    /// Character cap on the returned text; the oldest output is cut first. Default 32000, capped at 100000.
    pub max_chars: Option<usize>,
}

pub async fn run(inner: &Inner, params: ReadParams) -> Result<Value, HerdrError> {
    let (client, active) = inner.session()?;
    let target = match params.target {
        Some(target) => target,
        None => inner
            .resolve_caller(&client, &active)
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
    let rows = rows_for(lines);
    let (fetched, agent) = tokio::join!(fetch(&client, &target, source, rows), async {
        match inner.client() {
            Ok(client) => client.agent_get(&target).await.ok(),
            Err(_) => None,
        }
    });
    let status = agent.map(|agent| agent.agent_status);
    let (mut result, used) = fetched?;
    // A full-screen agent's history is read by scrolling it, so the first read asks for few rows
    // and one more is made only when blank lines left too few and more exists.
    if used == Source::Recent
        && result.truncated
        && rows < MAX_LINES
        && meaningful_count(&result.text) < lines as usize
        && let (more, Source::Recent) = fetch(
            &client,
            &target,
            used,
            rows.saturating_mul(3).min(MAX_LINES),
        )
        .await?
    {
        result = more;
    }
    if used != source {
        source = used;
        notes.push(
            "agent is busy in a full-screen UI, so scrollback is unavailable; returned the visible screen"
                .to_string(),
        );
    }

    let clean = last_meaningful(&strip_ansi(&result.text), lines as usize);
    let (text, cut) = truncate_tail(clean, max_chars);
    if cut {
        notes.push(format!("text cut to the last {max_chars} characters"));
    }
    let more_above = result.truncated || meaningful_count(&result.text) > lines as usize;
    let mut out = json!({
        "pane_id": result.pane_id,
        "source": source.herdr(),
        "lines": lines,
        "text": text,
        "truncated": cut || requested > lines,
        "more_above": more_above,
    });
    if let Some(status) = status {
        out["status"] = json!(status);
    }
    if !notes.is_empty() {
        out["note"] = Value::String(notes.join("; "));
    }
    Ok(out)
}

/// Reads an agent, or a plain pane when `target` is no agent. A busy full-screen agent has no
/// scrollback, so `recent` falls back to the visible screen; the source used comes back.
async fn fetch(
    client: &Client,
    target: &str,
    source: Source,
    rows: u32,
) -> Result<(PaneReadResult, Source), HerdrError> {
    match read_agent(client, target, source, rows).await {
        Err(HerdrError::Api {
            code: ApiErrorCode::AgentNotIdle,
            ..
        }) if source == Source::Recent => Ok((
            read_agent(client, target, Source::Visible, rows).await?,
            Source::Visible,
        )),
        Err(HerdrError::Api {
            code: ApiErrorCode::AgentNotFound,
            ..
        }) => Ok((read_pane(client, target, source, rows).await?, source)),
        other => Ok((other?, source)),
    }
}

/// Terminal rows to ask for to fill `lines` meaningful lines: a margin for blank lines and UI
/// rules, since every row of a full-screen agent's history costs a scroll.
fn rows_for(lines: u32) -> u32 {
    lines
        .saturating_add(lines / 2)
        .saturating_add(10)
        .min(MAX_LINES)
}

fn meaningful_count(text: &str) -> usize {
    strip_ansi(text)
        .lines()
        .filter(|line| is_meaningful(line))
        .count()
}

/// The last `lines` meaningful lines of a pane's screen, for a listing; `None` when it cannot be
/// read. The visible screen is read passively, so a listing never scrolls an agent.
pub async fn tail(client: &Client, target: &str, lines: u32) -> Option<Vec<String>> {
    let rows = lines.saturating_mul(4).min(MAX_LINES);
    let (result, _) = fetch(client, target, Source::Visible, rows).await.ok()?;
    let text = last_meaningful(&strip_ansi(&result.text), lines as usize);
    Some(text.lines().map(str::to_owned).collect())
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
