//! The tunnel audit log: one JSON line per tool call, kept apart from `herdr-mcp.log`.
//! It deliberately holds commands, text and keys, so the file is 0600 in a 0700 directory.
//! Pane output, read text and tool results never reach it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::tools::{close, interrupt, launch, mail, read, run, send, sessions, wait};

pub const LOG_NAME: &str = "tunnel-audit.log";

/// Where the audit log lives under `state_home` (`XDG_STATE_HOME`).
pub fn log_path(state_home: &Path) -> PathBuf {
    state_home.join("herdr-mcp").join(LOG_NAME)
}

/// What one call asked for: its target and the parameters worth recording.
#[derive(Debug, Default)]
pub struct Note {
    target: Option<String>,
    extra: Map<String, Value>,
}

fn value(item: &impl Serialize) -> Value {
    serde_json::to_value(item).unwrap_or(Value::Null)
}

impl Note {
    pub fn none() -> Self {
        Self::default()
    }

    fn target(mut self, target: Option<&str>) -> Self {
        self.target = target.map(str::to_owned);
        self
    }

    fn with(mut self, key: &str, item: Option<Value>) -> Self {
        if let Some(item) = item {
            self.extra.insert(key.to_owned(), item);
        }
        self
    }

    pub fn read(params: &read::ReadParams) -> Self {
        Self::none()
            .target(params.target.as_deref())
            .with("lines", params.lines.map(Value::from))
    }

    pub fn wait(params: &wait::WaitParams) -> Self {
        Self::none().target(params.target.as_deref())
    }

    pub fn mail_read(from: Option<&str>) -> Self {
        Self::none().target(from)
    }

    pub fn run(params: &run::RunParams) -> Self {
        Self::none().with("command", Some(json!(params.command)))
    }

    pub fn send(params: &send::SendParams) -> Self {
        Self::none()
            .target(Some(&params.target))
            .with("text", params.text.as_ref().map(|t| json!(t)))
            .with("keys", params.keys.as_ref().map(value))
            .with("prompt", params.prompt.as_ref().map(|p| json!(p)))
    }

    pub fn launch(params: &launch::LaunchParams) -> Self {
        Self::none()
            .with("kind", Some(json!(params.kind)))
            .with("name", Some(json!(params.name)))
            .with(
                "instructions",
                params.instructions.as_ref().map(|i| json!(i)),
            )
            .with("runtime_mode", params.runtime_mode.as_ref().map(value))
    }

    pub fn interrupt(params: &interrupt::InterruptParams) -> Self {
        Self::none()
            .target(Some(&params.target))
            .with("method", params.method.as_ref().map(value))
    }

    pub fn close(params: &close::CloseParams) -> Self {
        Self::none()
            .target(Some(&params.target))
            .with("force", params.force.map(Value::from))
    }

    /// The body is private mail: only its length is recorded.
    pub fn mail_send(params: &mail::SendParams) -> Self {
        Self::none()
            .with("to", Some(json!(params.to)))
            .with("body_len", Some(json!(params.body.len())))
    }

    pub fn use_session(params: &sessions::UseParams) -> Self {
        Self::none().with("name", Some(json!(params.name)))
    }
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
