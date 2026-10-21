use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herdr::{Client, HerdrError, PaneProcessInfo, PaneSendInputParams};
use crate::matcher::{MatchOutcome, Matcher, POLL_INTERVAL, read_text, tail, wait_for_match};
use crate::place::{self, Options, Placed, Where};
use crate::scope::Scope;
use crate::server::Inner;
use crate::tools::launch_session::LaunchSession;
use crate::tools::{DEFAULT_TIMEOUT, clamp_timeout};

const PGID_OBSERVE_WINDOW: Duration = Duration::from_secs(2);
const STABLE_FOR: Duration = Duration::from_millis(200);

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
        return Err(HerdrError::invalid_params("command must not be empty"));
    }
    if params
        .ready
        .as_ref()
        .is_some_and(|ready| ready.pattern.is_empty())
    {
        return Err(HerdrError::invalid_params("ready.match must not be empty"));
    }
    let options = Options {
        remote: inner.is_remote(),
        cwd: params.cwd,
        env: params.env.unwrap_or_default(),
        label: params.label,
    };
    place::validate(params.placement.as_ref(), &options)?;
    let mut scope = Scope::open(inner)?;
    let resolution = scope.caller().await?.clone();
    let client = scope.client();
    let placed = place::place(client, &resolution, params.placement.as_ref(), &options).await?;
    let session = LaunchSession::new(client, placed, "");
    let placed = session.placed();

    let needle = params
        .ready
        .as_ref()
        .map_or("", |ready| ready.pattern.as_str());
    let matcher = match send(client, placed, &params.command, needle).await {
        Ok(matcher) => matcher,
        Err(err) => return Err(session.fail_and_clean_up(err).await),
    };

    let cap = inner.profile.wait_cap(client_info_name);
    let mut note = None;
    let timeout = match &params.ready {
        Some(ready) => {
            let (timeout, capped) = clamp_timeout(ready.timeout_ms, cap, DEFAULT_TIMEOUT);
            note = capped;
            timeout
        }
        None => PGID_OBSERVE_WINDOW,
    };

    let tracker = Arc::new(Mutex::new(Tracker::new(first_word(&params.command))));
    let observer = tokio::spawn(observe_pgid(
        inner.client()?,
        placed.pane_id.clone(),
        Arc::clone(&tracker),
        timeout.min(PGID_OBSERVE_WINDOW),
    ));

    let abort = observer.abort_handle();
    let (ready_matched, timed_out, excerpt) = if params.ready.is_some() {
        let outcome = wait_for_match(&matcher, timeout, POLL_INTERVAL, || {
            read_text(client, &placed.pane_id)
        })
        .await?;
        match outcome {
            MatchOutcome::Matched { excerpt } => (Some(true), false, excerpt),
            MatchOutcome::TimedOut { tail } => (Some(false), true, tail),
        }
    } else {
        let _ = observer.await;
        let text = read_text(client, &placed.pane_id).await.unwrap_or_default();
        (None, false, tail(&text))
    };
    abort.abort();

    if tracker.lock().unwrap().pgid().is_none()
        && let Ok(info) = client.pane_process_info(&placed.pane_id).await
    {
        tracker.lock().unwrap().observe(&info, Instant::now());
    }
    let pgid = tracker.lock().unwrap().pgid();
    let name = tracker.lock().unwrap().name.clone();
    if pgid.is_none() {
        let unmatched = format!(
            "no foreground process group was identified as {name:?}; pgid is null, so do not wait on a process exit"
        );
        note = Some(note.map_or(unmatched.clone(), |n| format!("{n}; {unmatched}")));
    }

    let mut out = json!({
        "pane": placed.pane_id,
        "tab": placed.tab_id,
        "workspace": placed.workspace_id,
        "cwd": placed.cwd,
        "created": placed.created_json(),
        "pgid": pgid,
        "timed_out": timed_out,
        "excerpt": excerpt,
    });
    if let Some(matched) = ready_matched {
        out["ready_matched"] = json!(matched);
    }
    if let Some(warning) = &placed.warning {
        out["warning"] = json!(warning);
    }
    if timed_out {
        out["rewait"] = json!({
            "target": placed.pane_id,
            "output": {"match": needle, "after_command": params.command},
        });
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

/// Basename of the command's first word, skipping `env` and `VAR=value` assignments.
fn first_word(command: &str) -> String {
    command
        .split_whitespace()
        .find(|word| *word != "env" && !is_assignment(word))
        .map(|word| word.rsplit('/').next().unwrap_or(word).to_string())
        .unwrap_or_default()
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Picks the command's process group among the shell's prompt helpers: the group holding a
/// process named like the command, else one that stayed in front across polls 200 ms apart.
struct Tracker {
    name: String,
    matched: Option<u32>,
    stable: Option<u32>,
    candidate: Option<(u32, Instant)>,
}

impl Tracker {
    fn new(name: String) -> Self {
        Self {
            name,
            matched: None,
            stable: None,
            candidate: None,
        }
    }

    fn pgid(&self) -> Option<u32> {
        self.matched.or(self.stable)
    }

    fn observe(&mut self, info: &PaneProcessInfo, now: Instant) {
        let Some(front) = info.left_shell_group() else {
            self.candidate = None;
            return;
        };
        if self.matched.is_none() && self.names_command(info) {
            self.matched = Some(front);
        }
        match self.candidate {
            Some((group, since)) if group == front => {
                if self.stable.is_none() && now.duration_since(since) >= STABLE_FOR {
                    self.stable = Some(front);
                }
            }
            _ => self.candidate = Some((front, now)),
        }
    }

    fn names_command(&self, info: &PaneProcessInfo) -> bool {
        let base = |text: &str| text.rsplit('/').next().unwrap_or(text).to_string();
        !self.name.is_empty()
            && info.foreground_processes.iter().any(|process| {
                process.name == self.name
                    || process.argv0.as_deref().map(base).as_deref() == Some(&self.name)
                    || process
                        .argv
                        .as_ref()
                        .and_then(|argv| argv.first())
                        .map(|arg| base(arg))
                        .as_deref()
                        == Some(&self.name)
            })
    }
}

async fn observe_pgid(
    client: Client,
    pane_id: String,
    tracker: Arc<Mutex<Tracker>>,
    window: Duration,
) {
    let deadline = Instant::now() + window;
    while tracker.lock().unwrap().matched.is_none() && Instant::now() < deadline {
        if let Ok(info) = client.pane_process_info(&pane_id).await {
            tracker.lock().unwrap().observe(&info, Instant::now());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
