use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herdr::{
    AgentInfo, AgentReadParams, ApiErrorCode, Client, HerdrError, PaneReadParams, PaneReadResult,
    ReadSource,
};
use crate::output::{is_meaningful, last_meaningful, strip_ansi, truncate_tail};
use crate::server::Inner;

mod session;

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
    Session,
    Conversation,
}

impl Source {
    fn herdr(self) -> Result<ReadSource, HerdrError> {
        match self {
            Self::Recent => Ok(ReadSource::Recent),
            Self::Visible => Ok(ReadSource::Visible),
            Self::Session | Self::Conversation => Err(HerdrError::invalid_params(
                "session and conversation output are not terminal sources",
            )),
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadParams {
    /// Pane ID or agent name. Defaults to the calling pane.
    pub target: Option<String>,
    /// The default first tries saved `conversation` text for a working agent with a local session, then terminal `recent`. Explicit `recent` reads terminal history; `visible` reads the screen; `session` reads saved assistant text; `conversation` reads saved user and assistant text. Saved data can lag and omits tools/reasoning.
    pub source: Option<Source>,
    /// For `source: session` or `conversation`: pass that view's `next_cursor` to read newly appended complete records. Omit for the recent tail. This is an incremental cursor, not pagination of text cut by lines/max_chars.
    pub after: Option<String>,
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
    let implicit_source = params.source.is_none();
    let source = params.source.unwrap_or(Source::Recent);
    if matches!(source, Source::Session | Source::Conversation) {
        return read_session(
            inner,
            &client,
            &target,
            params.after,
            requested,
            lines,
            max_chars,
            source == Source::Conversation,
        )
        .await;
    }
    if params.after.is_some() {
        return Err(HerdrError::invalid_params(
            "after requires source: session or conversation",
        ));
    }
    if implicit_source
        && inner.install_env.is_some()
        && let Ok(agent) = client.agent_get(&target).await
        && agent.agent_status == crate::herdr::AgentStatus::Working
        && agent.agent_session.is_some()
        && let Ok(mut saved) = read_session_for_agent(
            inner, &client, agent, None, requested, lines, max_chars, true,
        )
        .await
        && !saved["text"].as_str().unwrap_or_default().is_empty()
    {
        let note = saved["note"].as_str().unwrap_or_default();
        saved["note"] = json!(format!(
            "working agent: selected saved conversation without terminal read; {note}; use source: visible for live dialogs"
        ));
        return Ok(saved);
    }
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
    // and one more is made only when blank lines left too few and more exists. If that one
    // fails, the first read stands.
    if used == Source::Recent
        && result.truncated
        && rows < MAX_LINES
        && meaningful_count(&result.text) < lines as usize
        && let Ok((more, Source::Recent)) = fetch(
            &client,
            &target,
            used,
            rows.saturating_mul(2).min(MAX_LINES),
        )
        .await
    {
        result = more;
    }
    if used != source {
        notes.push(
            "agent is busy in a full-screen UI; only the visible screen was returned. Terminal history was not inspected. For saved assistant text, use source: session (which can lag and omits tools)"
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
        "source": used.herdr()?,
        "lines": lines,
        "text": text,
        "truncated": cut || requested > lines,
        "more_above": more_above,
    });
    if used == Source::Visible && source == Source::Recent {
        out["terminal_history_unavailable"] = json!(true);
    }
    if let Some(status) = status {
        out["status"] = json!(status);
    }
    if !notes.is_empty() {
        out["note"] = Value::String(notes.join("; "));
    }
    Ok(out)
}

async fn read_session(
    inner: &Inner,
    client: &Client,
    target: &str,
    after: Option<String>,
    requested: u32,
    lines: u32,
    max_chars: usize,
    include_user: bool,
) -> Result<Value, HerdrError> {
    let agent = client.agent_get(target).await?;
    read_session_for_agent(
        inner,
        client,
        agent,
        after,
        requested,
        lines,
        max_chars,
        include_user,
    )
    .await
}

async fn read_session_for_agent(
    inner: &Inner,
    client: &Client,
    agent: AgentInfo,
    after: Option<String>,
    requested: u32,
    lines: u32,
    max_chars: usize,
    include_user: bool,
) -> Result<Value, HerdrError> {
    let env = inner.install_env.clone().ok_or_else(|| {
        HerdrError::tool(
            "session_unavailable",
            "agent data directories are unavailable",
        )
    })?;
    let snapshot = agent.clone();
    let output = tokio::task::spawn_blocking(move || {
        session::read(&snapshot, &env, after.as_deref(), include_user)
    })
    .await
    .map_err(|_| HerdrError::tool("session_unavailable", "session reader did not finish"))??;
    // A pane may have been reused or switched sessions while its file was read.
    let current = client.agent_get(&agent.pane_id).await?;
    if current.terminal_id != agent.terminal_id
        || current.agent != agent.agent
        || current.agent_session != agent.agent_session
    {
        return Err(HerdrError::tool(
            "session_changed",
            "the pane changed sessions during the read; read again without a cursor",
        ));
    }
    let clean = strip_ansi(&output.text);
    let more_above = output.more_above || meaningful_count(&clean) > lines as usize;
    let (text, cut) = truncate_tail(last_meaningful(&clean, lines as usize), max_chars);
    let mut notes = vec![if include_user {
        "saved user and assistant text only; tools, reasoning, and other context are omitted. Saved output can lag live output; status comes from Herdr".to_string()
    } else {
        "saved assistant text only; prompts, tools and reasoning are omitted. Saved output can lag live output; status comes from Herdr".to_string()
    }];
    if requested > lines {
        notes.push(format!("lines capped at {MAX_LINES}"));
    }
    if cut {
        notes.push(format!("text cut to the last {max_chars} characters"));
    }
    if output.pending {
        notes.push("an unfinished transcript record is pending; the cursor will retry it".into());
    }
    Ok(json!({
        "pane_id": current.pane_id, "source": if include_user { "conversation" } else { "session" }, "provider": output.provider,
        "session_id": output.session_id, "status": current.agent_status,
        "content_scope": if include_user { "user_assistant_text" } else { "assistant_text" },
        "context_complete": false, "freshness": "saved", "transcript_pending": output.pending,
        "lines": lines, "text": text, "truncated": cut || requested > lines,
        "more_above": more_above, "more_after": output.more_after,
        "next_cursor": output.next_cursor, "note": notes.join("; "),
    }))
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
    params.source = source.herdr()?;
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
    params.source = source.herdr()?;
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
