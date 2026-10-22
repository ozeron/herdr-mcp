//! `herdr_file_read`: a file under the directory a pane was opened in, in pages. File contents
//! are never logged.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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

/// Refused under any pane, along with `.env` files. A speed bump, not a boundary: an agent with
/// a shell can read these.
const SECRETS: &[&str] = &[".ssh", ".gnupg", ".aws", ".netrc", ".git-credentials"];

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FileReadParams {
    /// Pane ID or agent name. Files are read from the directory this pane was opened in.
    pub target: String,
    /// Path relative to the pane's directory, or an absolute path inside it.
    pub path: String,
    /// Byte offset to start at. To continue a file, pass the previous next_offset. Default 0.
    pub offset: Option<u64>,
    /// Bytes to return. Default 50000, capped at 75000.
    pub max_bytes: Option<usize>,
}

pub async fn run(inner: &Inner, params: FileReadParams) -> Result<Value, HerdrError> {
    let (client, _) = inner.session()?;
    let snapshot = client.session_snapshot().await?;
    let root = pane_root(&snapshot, &params.target)?;
    let asked = Path::new(&params.path);
    let real = if asked.is_absolute() {
        real_path(&params.path)
    } else {
        real_path(&root.join(asked).to_string_lossy())
    };
    check(&real, &root, &params.path)?;
    let offset = params.offset.unwrap_or(0);
    let max_bytes = params
        .max_bytes
        .unwrap_or(DEFAULT_BYTES)
        .clamp(1, MAX_BYTES);
    tokio::task::spawn_blocking(move || read_page(&real, &root, &params.path, offset, max_bytes))
        .await
        .map_err(|err| HerdrError::tool("file_unreadable", err.to_string()))?
}

/// The real directory `target`'s pane was opened in. A pane in `/` or the home directory is
/// refused: that would open everything.
fn pane_root(snapshot: &SessionSnapshot, target: &str) -> Result<PathBuf, HerdrError> {
    let pane_id = snapshot
        .agents
        .iter()
        .find(|agent| agent.name.as_deref() == Some(target))
        .map_or(target, |agent| agent.pane_id.as_str());
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .ok_or_else(|| {
            HerdrError::tool(
                "target_not_found",
                format!("no pane or agent matches {target:?}"),
            )
        })?;
    let cwd = pane.cwd.as_deref().ok_or_else(|| {
        HerdrError::tool(
            "no_directory",
            format!("Herdr reports no directory for {pane_id}"),
        )
    })?;
    let root = real_path(cwd);
    let home = std::env::var_os("HOME").map(|home| real_path(&home.to_string_lossy()));
    if root.parent().is_none() || home.is_some_and(|home| home.starts_with(&root)) {
        return Err(HerdrError::tool(
            "path_not_allowed",
            format!(
                "{pane_id} was opened in {}, which would expose everything under it; read from a pane opened in a project",
                root.display()
            ),
        ));
    }
    Ok(root)
}

/// Errors name the path as asked, so a symlink's target is not disclosed.
fn check(real: &Path, root: &Path, asked: &str) -> Result<(), HerdrError> {
    if !real.starts_with(root) {
        return Err(HerdrError::tool(
            "path_not_allowed",
            format!("{asked} is outside {}", root.display()),
        ));
    }
    let secret = real.components().any(|part| match part {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            // Lowercased: macOS paths ignore case, so .SSH is .ssh.
            let name = name.to_lowercase();
            SECRETS.contains(&name.as_str()) || name == ".env" || name.starts_with(".env.")
        }
        _ => false,
    });
    if secret {
        return Err(HerdrError::tool(
            "path_not_allowed",
            format!("{asked} may hold credentials, so it is not read"),
        ));
    }
    Ok(())
}

/// Opens `real` without following a final symlink, then checks the path the open file really
/// has: a directory swapped for a symlink after `check` is caught.
fn read_page(
    real: &Path,
    root: &Path,
    asked: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<Value, HerdrError> {
    let unreadable =
        |err: std::io::Error| HerdrError::tool("file_unreadable", format!("{asked}: {err}"));
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(real)
        .map_err(|err| match err.kind() {
            ErrorKind::NotFound => {
                HerdrError::tool("file_not_found", format!("{asked} does not exist"))
            }
            _ => unreadable(err),
        })?;
    let opened = open_path(&file).ok_or_else(|| {
        HerdrError::tool(
            "file_unreadable",
            format!("{asked}: the open file's real path cannot be checked here"),
        )
    })?;
    check(&opened, root, asked)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(HerdrError::tool(
            "not_a_file",
            format!("{asked} is not a regular file"),
        ));
    }
    // A hard link has an inside name for a file that may live anywhere.
    if meta.nlink() > 1 {
        return Err(HerdrError::tool(
            "path_not_allowed",
            format!("{asked} has other hard links, so where it lives cannot be checked"),
        ));
    }
    if offset > meta.len() {
        return Err(HerdrError::invalid_params(format!(
            "offset {offset} is past the end of {asked} ({} bytes)",
            meta.len()
        )));
    }
    file.seek(SeekFrom::Start(offset)).map_err(unreadable)?;
    // Three bytes past the page, so it can end on a whole character.
    let want = max_bytes + 3;
    let mut buf = Vec::with_capacity(want);
    file.take(want as u64)
        .read_to_end(&mut buf)
        .map_err(unreadable)?;

    let end = char_end(&buf, max_bytes);
    let (encoding, content) = match std::str::from_utf8(&buf[..end]) {
        Ok(text) => ("utf8", text.to_owned()),
        Err(_) => ("base64", STANDARD.encode(&buf[..end])),
    };
    let eof = buf.len() < want && end == buf.len();
    let next = offset + end as u64;
    Ok(json!({
        "path": real,
        "size": meta.len().max(offset + buf.len() as u64),
        "offset": offset,
        "bytes": end,
        "encoding": encoding,
        "content": content,
        "next_offset": if eof { Value::Null } else { json!(next) },
        "eof": eof,
    }))
}

/// Where a page of at most `max_bytes` ends: back to the last character boundary, or on to the
/// first when the page is shorter than one character.
fn char_end(buf: &[u8], max_bytes: usize) -> usize {
    let boundary = |at: usize| buf.get(at).is_none_or(|byte| byte & 0xC0 != 0x80);
    let mut end = buf.len().min(max_bytes);
    while end > 0 && !boundary(end) {
        end -= 1;
    }
    if end == 0 && !buf.is_empty() {
        end = 1;
        while !boundary(end) {
            end += 1;
        }
    }
    end
}

/// The path an open file really has, or `None` where the platform cannot say; then nothing is
/// read.
#[cfg(target_os = "macos")]
fn open_path(file: &File) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into `buf`, which has that length.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
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
