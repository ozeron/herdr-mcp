//! File-backed agent mailbox. Every bridge process shares the filesystem, so there is no daemon.
//!
//! Layout: `<state>/herdr-mcp/mailbox/<session_key>/<recipient_key>/{new,read}/<id>.json`, plus a
//! `.nudged` marker. A recipient key is a pane key (`w1_p2`) or, for an agent with a Herdr name,
//! a name key (`name-<name>`), so mail survives the agent moving to another pane. A message is written to a temp file and renamed into `new/`, so a reader
//! never sees a partial one. A read renames files into `read/`; whoever wins the rename gets the
//! message.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::herdr::HerdrError;

pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const MAX_UNREAD: usize = 500;
pub const READ_RETENTION: Duration = Duration::from_secs(24 * 3600);

const NEW: &str = "new";
const READ: &str = "read";
const NUDGED: &str = ".nudged";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub pane: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub ts_ms: u64,
    pub from: Party,
    pub to: Party,
    pub body: String,
    pub reply_to: Option<String>,
}

/// The state directory: `$XDG_STATE_HOME` when it is an absolute path, else `~/.local/state`.
pub fn default_state_home() -> PathBuf {
    let xdg = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    xdg.or_else(|| {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|home| PathBuf::from(home).join(".local/state"))
    })
    .unwrap_or_else(|| std::env::temp_dir().join("herdr-mcp-state"))
}

/// The first 12 hex characters of an FNV-1a 64 hash of the socket path.
pub fn session_key(socket: &Path) -> String {
    format!("{:016x}", fnv(socket.as_os_str().as_bytes()))[..12].to_string()
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn sanitise(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A pane ID made safe for a directory name: `w1:p2` becomes `w1_p2`.
pub fn recipient_key(pane: &str) -> String {
    let key = sanitise(pane);
    match key.as_str() {
        "" | "." | ".." => "_".into(),
        _ => key,
    }
}

/// The key of an agent's name inbox: `name-<sanitised name>`. Names that differ only in
/// characters outside `[A-Za-z0-9._-]` share an inbox; a name over 64 characters is cut and
/// suffixed with a hash of the whole name.
pub fn name_key(name: &str) -> String {
    let key = sanitise(name);
    if key.chars().count() <= 64 {
        return format!("name-{key}");
    }
    let head: String = key.chars().take(64).collect();
    format!("name-{head}-{:08x}", fnv(name.as_bytes()) & 0xffff_ffff)
}

/// The recipient keys one agent reads: its pane inbox and, when it has a name, its name inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbox {
    keys: Vec<String>,
}

impl Inbox {
    /// The inbox of a name alone, for a caller with no pane.
    pub fn named(name: &str) -> Self {
        Self {
            keys: vec![name_key(name)],
        }
    }

    pub fn new(pane: &str, name: Option<&str>) -> Self {
        let mut keys = vec![recipient_key(pane)];
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            keys.push(name_key(name));
        }
        Self { keys }
    }

    pub fn keys(&self) -> &[String] {
        &self.keys
    }
}

/// Matches mail by sender: any of `ids` equal to the sender's pane ID or agent name, and, with
/// `since`, only mail sent at or after that time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderFilter {
    ids: Vec<String>,
    since_ms: Option<u64>,
}

impl SenderFilter {
    pub fn new(ids: impl IntoIterator<Item = String>) -> Self {
        Self {
            ids: ids.into_iter().collect(),
            since_ms: None,
        }
    }

    /// Leaves out mail sent before `ts_ms`, such as an earlier answer still unread.
    #[must_use]
    pub fn since(mut self, ts_ms: u64) -> Self {
        self.since_ms = Some(ts_ms);
        self
    }

    pub fn matches(&self, message: &Message) -> bool {
        let from = &message.from;
        self.since_ms.is_none_or(|since| message.ts_ms >= since)
            && self.ids.iter().any(|id| {
                from.pane.as_deref() == Some(id.as_str())
                    || from.name.as_deref() == Some(id.as_str())
            })
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A sortable ID: `<ts_ms>-<pid>-<counter>`, zero padded.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:013}-{:010}-{:06}",
        now_ms(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed) % 1_000_000
    )
}

pub fn check_body(body: &str) -> Result<(), HerdrError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(HerdrError::tool(
            "body_too_large",
            format!(
                "the body is {} bytes; the limit is {MAX_BODY_BYTES}",
                body.len()
            ),
        ));
    }
    Ok(())
}

fn io_error(what: &str, err: &io::Error) -> HerdrError {
    HerdrError::tool("mailbox_io", format!("{what}: {}", err.kind()))
}

#[derive(Debug, Clone)]
pub struct Mailbox {
    root: PathBuf,
}

impl Mailbox {
    /// `state_home` is what `XDG_STATE_HOME` would be; `socket` is the resolved Herdr socket.
    pub fn new(state_home: &Path, socket: &Path) -> Self {
        Self {
            root: state_home
                .join("herdr-mcp")
                .join("mailbox")
                .join(session_key(socket)),
        }
    }

    /// The directory of one recipient key.
    pub fn dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    fn ensure_inbox(&self, key: &str) -> Result<PathBuf, HerdrError> {
        let inbox = self.dir(key);
        for dir in [inbox.join(NEW), inbox.join(READ)] {
            make_private_dir(&dir).map_err(|e| io_error("cannot create the inbox", &e))?;
        }
        Ok(inbox)
    }

    /// Unread messages in `inbox`, from `from` only when given.
    pub fn unread_count(&self, inbox: &Inbox, from: Option<&SenderFilter>) -> usize {
        self.unread(inbox, from).len()
    }

    fn unread_in(&self, key: &str) -> usize {
        list(&self.dir(key).join(NEW)).len()
    }

    /// Unread files of every key, oldest first, with the message when `from` needs it parsed.
    fn unread(&self, inbox: &Inbox, from: Option<&SenderFilter>) -> Vec<Unread> {
        let mut found: Vec<Unread> = Vec::new();
        for key in inbox.keys() {
            let dir = self.dir(key);
            for path in list(&dir.join(NEW)) {
                let message = from.map(|_| read_message(&path));
                if let (Some(filter), Some(parsed)) = (from, &message)
                    && !parsed.as_ref().is_some_and(|m| filter.matches(m))
                {
                    continue;
                }
                found.push(Unread {
                    path,
                    read_dir: dir.join(READ),
                    message: message.flatten(),
                });
            }
        }
        found.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
        found
    }

    /// Writes one message into the inbox of recipient key `key`.
    pub fn deliver(&self, key: &str, message: &Message) -> Result<(), HerdrError> {
        check_body(&message.body)?;
        let inbox = self.ensure_inbox(key)?;
        let new = inbox.join(NEW);
        if list(&new).len() >= MAX_UNREAD {
            return Err(HerdrError::tool(
                "inbox_full",
                format!("{key} already has {MAX_UNREAD} unread messages"),
            ));
        }
        let json = serde_json::to_vec(message)
            .map_err(|e| HerdrError::tool("mailbox_io", format!("cannot encode: {e}")))?;
        let tmp = new.join(format!(".tmp-{}", message.id));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&json)?;
            file.flush()?;
            fs::rename(&tmp, new.join(format!("{}.json", message.id)))
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&tmp);
            return Err(io_error("cannot write the message", &err));
        }
        Ok(())
    }

    /// Oldest-first unread messages across the inbox's keys, at most `limit`, and how many
    /// matching unread remain. `from` leaves other senders' mail untouched. With `mark_read`
    /// the messages move to `read/` and the nudge markers are cleared.
    pub fn take(
        &self,
        inbox: &Inbox,
        from: Option<&SenderFilter>,
        mark_read: bool,
        limit: usize,
    ) -> Result<(Vec<Message>, usize), HerdrError> {
        for key in inbox.keys() {
            purge_old(&self.dir(key).join(READ));
        }
        let unread = self.unread(inbox, from);
        let total = unread.len();
        let mut messages = Vec::new();
        let mut handled = 0;
        for entry in unread.into_iter().take(limit) {
            handled += 1;
            let parsed = entry.message.or_else(|| read_message(&entry.path));
            if mark_read {
                let Some(name) = entry.path.file_name() else {
                    continue;
                };
                let dest = entry.read_dir.join(name);
                if fs::rename(&entry.path, &dest).is_err() {
                    // Another reader won this message.
                    continue;
                }
                if let Ok(file) = File::options().write(true).open(&dest) {
                    let _ = file.set_modified(SystemTime::now());
                }
            }
            messages.extend(parsed);
        }
        if mark_read {
            for key in inbox.keys() {
                let _ = fs::remove_file(self.dir(key).join(NUDGED));
            }
        }
        Ok((messages, total - handled))
    }

    /// Claims the right to nudge for recipient key `key`. `true` means no nudge is outstanding
    /// and the caller should send one; `false` means one was already sent and unread mail remains.
    pub fn claim_nudge(&self, key: &str) -> bool {
        let marker = self.dir(key).join(NUDGED);
        for _ in 0..2 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&marker)
            {
                Ok(_) => return true,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    // A marker with nothing unread is stale: the reader raced the sender.
                    if self.unread_in(key) > 0 {
                        return false;
                    }
                    let _ = fs::remove_file(&marker);
                }
                Err(_) => return false,
            }
        }
        false
    }

    pub fn release_nudge(&self, key: &str) {
        let _ = fs::remove_file(self.dir(key).join(NUDGED));
    }
}

struct Unread {
    path: PathBuf,
    read_dir: PathBuf,
    message: Option<Message>,
}

fn read_message(path: &Path) -> Option<Message> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Message>(&bytes).ok())
}

fn make_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // `mode` is subject to the umask and skipped for a directory that already exists.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Message files in `dir`, sorted by name (so oldest first). Temp and marker files are skipped.
fn list(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.') && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    files
}

fn purge_old(read: &Path) {
    let Some(cutoff) = SystemTime::now().checked_sub(READ_RETENTION) else {
        return;
    };
    for path in list(read) {
        let old = fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old {
            let _ = fs::remove_file(path);
        }
    }
}
