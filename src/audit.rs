//! The tunnel audit log: one JSON line per tool call, kept apart from `herdr-mcp.log`.
//! It deliberately holds commands, text and keys, so the file is 0600 in a 0700 directory.
//! Every argument is kept except mail bodies, output needles and env values (see `Note::args`).
//! Pane output, read text and tool results never reach it: a result adds only a short summary
//! (delivery, match flags, how many characters a read returned).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

pub const LOG_NAME: &str = "tunnel-audit.log";

/// Where the audit log lives under `state_home` (`XDG_STATE_HOME`).
pub fn log_path(state_home: &Path) -> PathBuf {
    state_home.join("herdr-mcp").join(LOG_NAME)
}

/// What one call asked for: its target and the parameters worth recording.
#[derive(Debug, Default, Clone)]
pub struct Note {
    target: Option<String>,
    extra: Map<String, Value>,
}

impl Note {
    pub fn none() -> Self {
        Self::default()
    }

    fn with(mut self, key: &str, item: Option<Value>) -> Self {
        if let Some(item) = item {
            self.extra.insert(key.to_owned(), item);
        }
        self
    }

    /// The call's arguments as sent, minus what must not be kept: a mail body and a wait or
    /// readiness needle keep only their length, and `env` keeps only its variable names. Prompts, text, keys and commands stay,
    /// because they are what the remote caller told this machine to do.
    pub fn args(mut self, args: &Map<String, Value>) -> Self {
        for (key, item) in args {
            match key.as_str() {
                "body" => {
                    let len = item.as_str().map_or(0, str::len);
                    self.extra.insert("body_len".into(), json!(len));
                }
                "env" => {
                    let names: Vec<&String> = item
                        .as_object()
                        .map(|env| env.keys().collect())
                        .unwrap_or_default();
                    self.extra.insert("env".into(), json!(names));
                }
                "target" => self.target = item.as_str().map(str::to_owned),
                // A wait or readiness needle is usually copied from pane output.
                "output" | "ready" => {
                    self.extra.insert(key.clone(), without_match(item));
                }
                // The result summary, set first, wins: it says what happened, not what was asked.
                _ => {
                    self.extra
                        .entry(key.clone())
                        .or_insert_with(|| item.clone());
                }
            }
        }
        self
    }

    /// Who called: the MCP client's name and its session, so two chats can be told apart.
    pub fn caller(self, client: Option<&str>, mcp_session: Option<&str>) -> Self {
        self.with("client", client.map(|c| json!(c)))
            .with("mcp_session", mcp_session.map(|s| json!(s)))
    }

    /// A short, content-free summary of a successful result.
    pub fn result(mut self, result: &Value) -> Self {
        for key in SUMMARY_KEYS {
            if let Some(item) = result.get(*key).filter(|item| is_scalar(item)) {
                self.extra.insert((*key).to_owned(), item.clone());
            }
        }
        if let Some(text) = result.get("text").and_then(Value::as_str) {
            self.extra
                .insert("chars".into(), json!(text.chars().count()));
        }
        if let Some(workspaces) = result.get("workspaces").and_then(Value::as_array) {
            let panes = workspaces
                .iter()
                .flat_map(|ws| ws["tabs"].as_array().into_iter().flatten())
                .map(|tab| tab["panes"].as_array().map_or(0, Vec::len))
                .sum::<usize>();
            self.extra.insert("panes".into(), json!(panes));
        }
        if result.get("turn").is_some_and(|turn| !turn.is_null()) {
            self.extra.insert("turn".into(), json!(true));
        }
        self
    }

    /// Why a call failed, capped.
    pub fn error(self, message: &str) -> Self {
        self.with(
            "error",
            Some(json!(crate::output::truncate_head(
                message,
                MAX_ERROR_CHARS
            ))),
        )
    }
}

/// `item` with any `match` string replaced by its length.
fn without_match(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(fields) = item.as_object_mut()
        && let Some(needle) = fields.remove("match")
    {
        fields.insert(
            "match_len".into(),
            json!(needle.as_str().map_or(0, str::len)),
        );
    }
    item
}

/// Result fields that say what happened without carrying pane output.
const SUMMARY_KEYS: &[&str] = &[
    "result",
    "delivery",
    "source",
    "truncated",
    "matched",
    "timed_out",
    "ready_matched",
    "phase",
    "status",
];
const MAX_ERROR_CHARS: usize = 300;

fn is_scalar(item: &Value) -> bool {
    matches!(item, Value::String(_) | Value::Bool(_) | Value::Number(_))
}

#[derive(Debug, Clone)]
pub struct Audit {
    path: PathBuf,
}

impl Audit {
    pub fn new(state_home: &Path) -> Self {
        Self {
            path: log_path(state_home),
        }
    }

    /// Appends one line. A failure prints one warning and never fails the tool call.
    pub fn record(&self, tool: &str, session: &str, outcome: &str, elapsed: Duration, note: &Note) {
        let mut line = note.extra.clone();
        line.insert("ts".into(), json!(iso8601(unix_now())));
        line.insert("tool".into(), json!(tool));
        line.insert("session".into(), json!(session));
        line.insert("outcome".into(), json!(outcome));
        line.insert("ms".into(), json!(elapsed.as_millis() as u64));
        if let Some(target) = &note.target {
            line.insert("target".into(), json!(target));
        }
        if let Err(err) = self.append(&Value::Object(line).to_string()) {
            eprintln!("herdr-mcp: cannot write the audit log: {err}");
        }
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(format!("{line}\n").as_bytes())
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `2027-01-15T08:00:00Z` for a Unix time.
pub fn iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
