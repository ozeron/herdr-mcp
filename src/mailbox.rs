//! File-backed agent mailbox. Every bridge process shares the filesystem, so there is no daemon.
//!
//! Layout: `<state>/herdr-mcp/mailbox/<session_key>/<recipient_key>/{new,read}/<id>.json`, plus a
//! `.nudged` marker. A message is written to a temp file and renamed into `new/`, so a reader
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
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in socket.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")[..12].to_string()
}

/// A pane ID made safe for a directory name: `w1:p2` becomes `w1_p2`.
pub fn recipient_key(pane: &str) -> String {
    let key: String = pane
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match key.as_str() {
        "" | "." | ".." => "_".into(),
        _ => key,
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

    pub fn inbox(&self, pane: &str) -> PathBuf {
        self.root.join(recipient_key(pane))
    }

    fn ensure_inbox(&self, pane: &str) -> Result<PathBuf, HerdrError> {
        let inbox = self.inbox(pane);
        for dir in [inbox.join(NEW), inbox.join(READ)] {
            make_private_dir(&dir).map_err(|e| io_error("cannot create the inbox", &e))?;
        }
        Ok(inbox)
    }

    pub fn unread_count(&self, pane: &str) -> usize {
        list(&self.inbox(pane).join(NEW)).len()
    }

    /// Writes one message into the recipient's inbox.
    pub fn deliver(&self, pane: &str, message: &Message) -> Result<(), HerdrError> {
        check_body(&message.body)?;
        let inbox = self.ensure_inbox(pane)?;
        let new = inbox.join(NEW);
        if list(&new).len() >= MAX_UNREAD {
            return Err(HerdrError::tool(
                "inbox_full",
                format!("{pane} already has {MAX_UNREAD} unread messages"),
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

    /// Oldest-first unread messages, at most `limit`, and how many unread remain.
    /// With `mark_read` they move to `read/` and the nudge marker is cleared.
    pub fn take(
        &self,
        pane: &str,
        mark_read: bool,
        limit: usize,
    ) -> Result<(Vec<Message>, usize), HerdrError> {
        let inbox = self.inbox(pane);
        let new = inbox.join(NEW);
        let read = inbox.join(READ);
        let unread = list(&new);
        let total = unread.len();
        purge_old(&read);
        let mut messages = Vec::new();
        let mut handled = 0;
        for path in unread.into_iter().take(limit) {
            handled += 1;
            let parsed = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Message>(&bytes).ok());
            if mark_read {
                let Some(name) = path.file_name() else {
                    continue;
                };
                let dest = read.join(name);
                if fs::rename(&path, &dest).is_err() {
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
            let _ = fs::remove_file(inbox.join(NUDGED));
        }
        Ok((messages, total - handled))
    }

    /// Claims the right to nudge. `true` means no nudge is outstanding and the caller should send
    /// one; `false` means one was already sent and unread mail remains.
    pub fn claim_nudge(&self, pane: &str) -> bool {
        let marker = self.inbox(pane).join(NUDGED);
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
                    if self.unread_count(pane) > 0 {
                        return false;
                    }
                    let _ = fs::remove_file(&marker);
                }
                Err(_) => return false,
            }
        }
        false
    }

    pub fn release_nudge(&self, pane: &str) {
        let _ = fs::remove_file(self.inbox(pane).join(NUDGED));
    }
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
