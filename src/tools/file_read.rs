//! `herdr_file_read`: a file under a Herdr pane's directory or the temp directory, in pages, so
//! a caller with no shared filesystem gets an agent's files without printing them to a screen.
//! File contents are never logged.

use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::herdr::{HerdrError, SessionSnapshot};
use crate::place::real_path;
use crate::server::Inner;

pub const DEFAULT_BYTES: usize = 50_000;
/// 100000 characters once base64-encoded, the most a read returns.
pub const MAX_BYTES: usize = 75_000;

/// Path components that hold credentials, refused under any root.
const SECRET_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", ".netrc", ".git-credentials"];

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FileReadParams {
    /// Absolute path, or one starting with `~`. It must be under the working directory of a pane in this Herdr session (a workspace or worktree), or under the temp directory.
    pub path: String,
    /// Byte offset to start at. To continue a file, pass the previous next_offset. Default 0.
    pub offset: Option<u64>,
    /// Bytes to return. Default 50000, capped at 75000.
    pub max_bytes: Option<usize>,
}

pub async fn run(inner: &Inner, params: FileReadParams) -> Result<Value, HerdrError> {
    let (client, _) = inner.session()?;
    let snapshot = client.session_snapshot().await?;
    let real = real_path(&params.path);
    let roots = allowed_roots(&snapshot);
    if !roots.iter().any(|root| real.starts_with(root)) {
        return Err(HerdrError::detailed(
            "path_not_allowed",
            format!(
                "{} is outside every Herdr pane's working directory and the temp directory",
                real.display()
            ),
            json!({"path": real, "roots": roots}),
        ));
    }
    if let Some(secret) = secret_dir(&real) {
        return Err(HerdrError::detailed(
            "path_not_allowed",
            format!("{secret} holds credentials, so it is never read"),
            json!({"path": real}),
        ));
    }
    let offset = params.offset.unwrap_or(0);
    let max_bytes = params
        .max_bytes
        .unwrap_or(DEFAULT_BYTES)
        .clamp(1, MAX_BYTES);
    let (size, bytes) = read_slice(&real, offset, max_bytes).await?;
    let (encoding, content, used) = encode(&bytes, offset + bytes.len() as u64 >= size);
    let next = offset + used as u64;
    let eof = next >= size;
    Ok(json!({
        "path": real,
        "size": size,
        "offset": offset,
        "bytes": used,
        "encoding": encoding,
        "content": content,
        "next_offset": if eof { Value::Null } else { json!(next) },
        "eof": eof,
    }))
}

/// The directories a file may be read from: each pane's and agent's working directory, and
/// the temp directory. A directory that is the home directory or holds it is left out, so a
/// shell opened in `~` does not open the whole home.
fn allowed_roots(snapshot: &SessionSnapshot) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(|home| real_path(&home.to_string_lossy()));
    let panes = snapshot
        .panes
        .iter()
        .flat_map(|pane| [&pane.cwd, &pane.foreground_cwd]);
    let agents = snapshot
        .agents
        .iter()
        .flat_map(|agent| [&agent.cwd, &agent.foreground_cwd]);
    let temp = [
        Some(std::env::temp_dir().to_string_lossy().into_owned()),
        Some("/tmp".to_owned()),
    ];
    let mut roots: Vec<PathBuf> = panes
        .chain(agents)
        .flatten()
        .cloned()
        .chain(temp.into_iter().flatten())
        .map(|dir| real_path(&dir))
        .filter(|root| root.components().count() > 1)
        .filter(|root| home.as_ref().is_none_or(|home| !home.starts_with(root)))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn secret_dir(path: &Path) -> Option<&str> {
    path.components().find_map(|part| match part {
        Component::Normal(name) => name
            .to_str()
            .and_then(|name| SECRET_DIRS.iter().find(|secret| **secret == name).copied()),
        _ => None,
    })
}

/// The file's size and up to `max_bytes` from `offset`.
async fn read_slice(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<(u64, Vec<u8>), HerdrError> {
    let not_found = || {
        HerdrError::tool(
            "file_not_found",
            format!("{} does not exist", path.display()),
        )
    };
    let io = |err: std::io::Error| {
        HerdrError::tool(
            "file_unreadable",
            format!("{} cannot be read: {err}", path.display()),
        )
    };
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => not_found(),
            _ => io(err),
        })?;
    if !meta.is_file() {
        return Err(HerdrError::tool(
            "not_a_file",
            format!("{} is not a regular file", path.display()),
        ));
    }
    let size = meta.len();
    if offset > size {
        return Err(HerdrError::invalid_params(format!(
            "offset {offset} is past the end of the file ({size} bytes)"
        )));
    }
    let mut file = tokio::fs::File::open(path).await.map_err(io)?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(io)?;
    let mut bytes = Vec::with_capacity(max_bytes);
    file.take(max_bytes as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(io)?;
    Ok((size, bytes))
}

/// Text stays text; a page that ends inside a character stops before it, so the next page starts
/// on a whole character. Anything else is base64. Returns the encoding, the content and the
/// bytes it covers.
fn encode(bytes: &[u8], at_end: bool) -> (&'static str, String, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => ("utf8", text.to_owned(), bytes.len()),
        Err(err) if err.error_len().is_none() && !at_end && err.valid_up_to() > 0 => {
            let used = err.valid_up_to();
            let text = std::str::from_utf8(&bytes[..used]).unwrap_or_default();
            ("utf8", text.to_owned(), used)
        }
        Err(_) => ("base64", STANDARD.encode(bytes), bytes.len()),
    }
}
