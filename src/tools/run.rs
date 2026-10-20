use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caller::resolve_caller;
use crate::herdr::{Client, HerdrError, PaneReadParams, PaneSendInputParams, ReadSource};
use crate::matcher::{MatchOutcome, Matcher, POLL_INTERVAL, READ_LINES, tail, wait_for_match};
use crate::output::strip_ansi;
use crate::place::{self, Options, Placed, Where};
use crate::server::Inner;

pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const OBSERVE_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct Ready {
    /// Text to look for in output added after the command was sent. The echoed command never matches.
    #[serde(rename = "match")]
    pub pattern: String,
    /// How long to wait, in milliseconds. Default 30000, capped at the wait cap.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RunParams {
    /// Shell command to type into the pane.
    pub command: String,
    /// Where to run it. Default: a new tab in the calling pane's workspace.
    #[serde(rename = "where")]
    pub placement: Option<Where>,
    /// Working directory for a new pane.
    pub cwd: Option<String>,
    /// Environment for a new pane.
    pub env: Option<HashMap<String, String>>,
    /// Name for a new tab, workspace or worktree.
    pub label: Option<String>,
    /// Wait for a readiness line. Without it the call returns once the process group is seen.
    pub ready: Option<Ready>,
}

pub async fn run(
    inner: &Inner,
    client_info_name: Option<&str>,
    params: RunParams,
) -> Result<Value, HerdrError> {
    if params.command.trim().is_empty() {
        return Err(HerdrError::tool(
            "invalid_params",
            "command must not be empty",
        ));
    }
    if params
        .ready
        .as_ref()
        .is_some_and(|ready| ready.pattern.is_empty())
    {
        return Err(HerdrError::tool(
            "invalid_params",
            "ready.match must not be empty",
        ));
    }
    let options = Options {
        cwd: params.cwd,
        env: params.env.unwrap_or_default(),
        label: params.label,
    };
    place::validate(params.placement.as_ref(), &options)?;
    let client = inner.client()?;
    let resolution = resolve_caller(&client, &inner.caller_env, inner.ancestry.as_ref()).await?;
    let placed = place::place(&client, &resolution, params.placement.as_ref(), &options).await?;

    let needle = params
        .ready
        .as_ref()
        .map_or("", |ready| ready.pattern.as_str());
    let sent = send(&client, &placed, &params.command, needle).await;
    let matcher = match sent {
        Ok(matcher) => matcher,
        Err(err) => {
            placed.cleanup(&client).await;
            return Err(err);
        }
    };

    let cap = inner.profile.wait_cap(client_info_name);
    let mut note = None;
    let timeout = params.ready.as_ref().map_or(OBSERVE_WINDOW, |ready| {
        let asked = ready
            .timeout_ms
            .map_or(DEFAULT_READY_TIMEOUT, Duration::from_millis);
        if asked > cap {
            note = Some(format!(
                "timeout_ms capped at the wait cap of {} ms",
                cap.as_millis()
            ));
        }
        asked.min(cap)
    });

    let pgid = Arc::new(AtomicU32::new(0));
    let observer = tokio::spawn(observe_pgid(
        inner.client()?,
        placed.pane_id.clone(),
        Arc::clone(&pgid),
        timeout.min(OBSERVE_WINDOW),
    ));

    let abort = observer.abort_handle();
    let (ready_matched, timed_out, excerpt) = if params.ready.is_some() {
        let outcome = wait_for_match(&matcher, timeout, POLL_INTERVAL, || {
            read_text(&client, &placed.pane_id)
        })
        .await?;
        match outcome {
            MatchOutcome::Matched { excerpt } => (Some(true), false, excerpt),
            MatchOutcome::TimedOut { tail } => (Some(false), true, tail),
        }
    } else {
        let _ = observer.await;
        let text = read_text(&client, &placed.pane_id)
            .await
            .unwrap_or_default();
        (None, false, tail(&text))
    };
    abort.abort();

    if pgid.load(Ordering::Relaxed) == 0 {
        record_front(&client, &placed.pane_id, &pgid).await;
    }

    let mut out = json!({
        "pane": placed.pane_id,
        "tab": placed.tab_id,
        "workspace": placed.workspace_id,
        "created": placed.created_json(),
        "pgid": match pgid.load(Ordering::Relaxed) { 0 => Value::Null, n => json!(n) },
        "timed_out": timed_out,
        "excerpt": excerpt,
    });
    if let Some(matched) = ready_matched {
        out["ready_matched"] = json!(matched);
    }
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

/// Snapshots the pane, then types the command and presses Enter.
async fn send(
    client: &Client,
    placed: &Placed,
    command: &str,
    needle: &str,
) -> Result<Matcher, HerdrError> {
    let baseline = read_text(client, &placed.pane_id).await?;
    client
        .pane_send_input(&PaneSendInputParams {
            pane_id: placed.pane_id.clone(),
            text: command.to_string(),
            keys: vec!["Enter".to_string()],
        })
        .await?;
    Ok(Matcher::new(&baseline, needle, command))
}

pub(crate) async fn read_text(client: &Client, pane_id: &str) -> Result<String, HerdrError> {
    let mut params = PaneReadParams::new(pane_id);
    params.source = ReadSource::RecentUnwrapped;
    params.lines = Some(READ_LINES);
    let read = client.pane_read(&params).await?;
    Ok(strip_ansi(&read.text))
}

/// Stores the first foreground process group that is not the shell's (KTD13).
async fn record_front(client: &Client, pane_id: &str, pgid: &AtomicU32) {
    let Ok(info) = client.pane_process_info(Some(pane_id)).await else {
        return;
    };
    if let (Some(shell), Some(front)) = (info.shell_pid, info.foreground_process_group_id)
        && front != shell
        && front != 0
    {
        pgid.store(front, Ordering::Relaxed);
    }
}

async fn observe_pgid(client: Client, pane_id: String, pgid: Arc<AtomicU32>, window: Duration) {
    let deadline = tokio::time::Instant::now() + window;
    while pgid.load(Ordering::Relaxed) == 0 && tokio::time::Instant::now() < deadline {
        record_front(&client, &pane_id, &pgid).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
