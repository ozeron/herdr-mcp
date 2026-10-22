//! `herdr_file_read`: a file under a Herdr pane's directory or the temp directory, in pages, so
//! a caller with no shared filesystem gets an agent's files without printing them to a screen.
//! File contents are never logged.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herdr::{HerdrError, SessionSnapshot};
use crate::place::real_path;
use crate::server::Inner;

pub const DEFAULT_BYTES: usize = 50_000;
/// 100000 characters once base64-encoded, the most a read returns.
pub const MAX_BYTES: usize = 75_000;
/// The longest UTF-8 character, read past a page so it can end on a whole character.
const CHAR_MAX: usize = 4;

/// Path components that hold credentials, refused under any root.
const SECRET_PARTS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    "Keychains",
];

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FileReadParams {
    /// Absolute path, or one starting with `~/`. It must be under the working directory of a pane in this Herdr session (a project or worktree in the home directory), or under the temp directory.
    pub path: String,
    /// Byte offset to start at. To continue a file, pass the previous next_offset. Default 0.
    pub offset: Option<u64>,
    /// Bytes to return. Default 50000, capped at 75000.
    pub max_bytes: Option<usize>,
}

pub async fn run(inner: &Inner, params: FileReadParams) -> Result<Value, HerdrError> {
    let asked = params.path;
    if !(asked.starts_with('/') || asked == "~" || asked.starts_with("~/")) {
        return Err(HerdrError::invalid_params(
            "path must be absolute or start with ~/",
        ));
    }
    let (client, _) = inner.session()?;
    let snapshot = client.session_snapshot().await?;
    let roots = allowed_roots(&snapshot);
    let real = real_path(&asked);
    check(&real, &roots, &asked)?;
    let offset = params.offset.unwrap_or(0);
    let max_bytes = params
        .max_bytes
        .unwrap_or(DEFAULT_BYTES)
        .clamp(1, MAX_BYTES);
    let page =
        tokio::task::spawn_blocking(move || read_page(&real, &roots, &asked, offset, max_bytes))
            .await
            .map_err(|err| {
                HerdrError::tool("file_unreadable", format!("the read failed: {err}"))
            })??;
    Ok(page.to_json())
}

/// Refuses a path outside every root or through a credential directory. Errors name the path as
/// asked, so a symlink's target is not disclosed.
fn check(real: &Path, roots: &[PathBuf], asked: &str) -> Result<(), HerdrError> {
    if !roots.iter().any(|root| real.starts_with(root)) {
        return Err(HerdrError::detailed(
            "path_not_allowed",
            format!(
                "{asked} is outside every Herdr pane's working directory in the home directory and the temp directory"
            ),
            json!({"path": asked, "roots": roots}),
        ));
    }
    if let Some(part) = secret_part(real) {
        return Err(HerdrError::detailed(
            "path_not_allowed",
            format!("{part} may hold credentials, so it is never read"),
            json!({"path": asked}),
        ));
    }
    Ok(())
}

/// The directories a file may be read from: the temp directory, and each pane's and agent's
/// working directory that lies inside the home directory, below a visible entry other than
/// `Library`. So a shell in `~`, `/`, `/etc`, `~/.config` or `~/Library` opens nothing.
fn allowed_roots(snapshot: &SessionSnapshot) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(|home| real_path(&home.to_string_lossy()));
    let temps: Vec<PathBuf> = [
        std::env::temp_dir().to_string_lossy().into_owned(),
        "/tmp".into(),
    ]
    .iter()
    .map(|dir| real_path(dir))
    .collect();
    let in_home = |dir: &Path| {
        let Some(home) = &home else { return false };
        let Ok(rest) = dir.strip_prefix(home) else {
            return false;
        };
        match rest.components().next() {
            Some(Component::Normal(first)) => {
                let first = first.to_string_lossy();
                !first.starts_with('.') && first != "Library"
            }
            _ => false,
        }
    };
    let panes = snapshot
        .panes
        .iter()
        .flat_map(|pane| [&pane.cwd, &pane.foreground_cwd]);
    let agents = snapshot
        .agents
        .iter()
        .flat_map(|agent| [&agent.cwd, &agent.foreground_cwd]);
    let mut roots: Vec<PathBuf> = panes
        .chain(agents)
        .flatten()
        .map(|dir| real_path(dir))
        .filter(|dir| in_home(dir))
        .chain(temps)
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn secret_part(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if let Some(part) = parts
        .iter()
        .find(|part| SECRET_PARTS.contains(&part.as_str()))
    {
        return Some(part.clone());
    }
    if parts.last().is_some_and(|name| name.starts_with(".env")) {
        return parts.last().cloned();
    }
    parts
        .ends_with(&[".git".to_owned(), "config".to_owned()])
        .then(|| ".git/config".to_owned())
}

/// One page of a file.
#[derive(Debug)]
struct Page {
    path: PathBuf,
    size: u64,
    offset: u64,
    bytes: usize,
    encoding: &'static str,
    content: String,
    eof: bool,
    note: Option<&'static str>,
}

impl Page {
    fn to_json(&self) -> Value {
        let next = self.offset + self.bytes as u64;
        let mut out = json!({
            "path": self.path,
            "size": self.size,
            "offset": self.offset,
            "bytes": self.bytes,
            "encoding": self.encoding,
            "content": self.content,
            "next_offset": if self.eof { Value::Null } else { json!(next) },
            "eof": self.eof,
        });
        if let Some(note) = self.note {
            out["note"] = json!(note);
        }
        out
    }
}

/// Opens `real` without following a symlink at its end, then checks the path the open file
/// really has, so a component swapped for a symlink after `check` is caught.
fn read_page(
    real: &Path,
    roots: &[PathBuf],
    asked: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<Page, HerdrError> {
    let io = |err: std::io::Error| {
        HerdrError::tool("file_unreadable", format!("{asked} cannot be read: {err}"))
    };
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(real)
        .map_err(|err| match err.kind() {
            ErrorKind::NotFound => {
                HerdrError::tool("file_not_found", format!("{asked} does not exist"))
            }
            _ if err.raw_os_error() == Some(libc::ELOOP) => HerdrError::tool(
                "path_not_allowed",
                format!("{asked} became a symlink after it was checked; read its target instead"),
            ),
            _ => io(err),
        })?;
    if let Some(opened) = open_path(&file) {
        check(&opened, roots, asked)?;
    }
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(HerdrError::tool(
            "not_a_file",
            format!("{asked} is not a regular file"),
        ));
    }
    if offset > meta.len() {
        return Err(HerdrError::invalid_params(format!(
            "offset {offset} is past the end of the file ({} bytes)",
            meta.len()
        )));
    }
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    let want = max_bytes + CHAR_MAX;
    let mut buf = Vec::with_capacity(want);
    file.take(want as u64).read_to_end(&mut buf).map_err(io)?;
    let file_ends = buf.len() < want;

    let skip = if offset > 0 { mid_char_skip(&buf) } else { 0 };
    let (encoding, content, used) = encode(&buf[skip..], max_bytes, file_ends);
    Ok(Page {
        path: real.to_path_buf(),
        size: meta.len().max(offset + buf.len() as u64),
        offset: offset + skip as u64,
        bytes: used,
        encoding,
        content,
        eof: file_ends && skip + used == buf.len(),
        note: (skip > 0)
            .then_some("offset was inside a character; the page starts at the next one"),
    })
}

/// Continuation bytes at the start of `buf` when skipping them leaves text: an offset that
/// landed inside a character.
fn mid_char_skip(buf: &[u8]) -> usize {
    let skip = buf
        .iter()
        .take(CHAR_MAX - 1)
        .take_while(|byte| *byte & 0xC0 == 0x80)
        .count();
    let rest = &buf[skip..];
    let text_follows = match std::str::from_utf8(rest) {
        Ok(text) => !text.is_empty(),
        Err(err) => err.valid_up_to() > 0,
    };
    if skip > 0 && text_follows { skip } else { 0 }
}

/// Text stays text, and a page ends on a whole character: one cut inside a character stops
/// before it, and a page smaller than one character takes the whole character. Anything else is
/// base64. `buf` may run up to `CHAR_MAX` bytes past `max_bytes`; `file_ends` says whether the
/// file ends with it. Returns the encoding, the content and the bytes it covers.
fn encode(buf: &[u8], max_bytes: usize, file_ends: bool) -> (&'static str, String, usize) {
    let end = buf.len().min(max_bytes);
    let base64 = || ("base64", STANDARD.encode(&buf[..end]), end);
    match std::str::from_utf8(&buf[..end]) {
        Ok(text) => ("utf8", text.to_owned(), end),
        Err(err) if err.error_len().is_some() => base64(),
        Err(err) if err.valid_up_to() > 0 => {
            let used = err.valid_up_to();
            let text = String::from_utf8_lossy(&buf[..used]).into_owned();
            ("utf8", text, used)
        }
        // A page shorter than its first character.
        Err(_) if !file_ends || buf.len() > end => {
            let width = char_width(buf[0]);
            match buf.get(..width).map(std::str::from_utf8) {
                Some(Ok(text)) => ("utf8", text.to_owned(), width),
                _ => base64(),
            }
        }
        Err(_) => base64(),
    }
}

fn char_width(first: u8) -> usize {
    match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// The path an open file really has, or `None` where the platform cannot say.
#[cfg(target_os = "macos")]
fn open_path(file: &File) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into `buf`, which has that length.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if rc == -1 {
        return None;
    }
    let len = buf.iter().position(|byte| *byte == 0)?;
    Some(PathBuf::from(OsStr::from_bytes(&buf[..len])))
}

#[cfg(target_os = "linux")]
fn open_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_path(_file: &File) -> Option<PathBuf> {
    None
}
