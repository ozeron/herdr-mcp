//! Passive readers for the local transcript formats used by Claude Code and Codex.
//! Only a live Herdr session reference can select a file; paths never come from MCP input.

use std::convert::Infallible;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr::{AgentInfo, HerdrError};
use crate::install::InstallEnv;
use crate::verified_file::{self, Error as FileError};

const MAX_SCAN_ENTRIES: usize = 100_000;
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 1024 * 1024;

pub(super) struct Output {
    pub provider: String,
    pub session_id: String,
    pub text: String,
    pub next_cursor: String,
    pub more_above: bool,
    pub more_after: bool,
    pub pending: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    provider: String,
    session_id: String,
    device: u64,
    inode: u64,
    offset: u64,
    #[serde(default)]
    include_user: bool,
}

fn unavailable(message: &str) -> HerdrError {
    HerdrError::tool("session_unavailable", message)
}

fn stale() -> HerdrError {
    HerdrError::tool(
        "stale_cursor",
        "the transcript or session changed; read again without a cursor",
    )
}

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

pub(super) fn read(
    agent: &AgentInfo,
    env: &InstallEnv,
    after: Option<&str>,
    include_user: bool,
) -> Result<Output, HerdrError> {
    let session = agent.agent_session.as_ref().ok_or_else(|| {
        unavailable("no session reference is available; Herdr's agent integration must report it")
    })?;
    let provider = session.agent.as_str();
    if !matches!(provider, "claude" | "codex")
        || agent.agent.as_deref() != Some(provider)
        || session.source != format!("herdr:{provider}")
        || session.kind != "id"
        || !valid_id(&session.value)
    {
        return Err(unavailable(
            "a Claude or Codex session ID from its Herdr integration is required",
        ));
    }
    let root = match provider {
        "claude" => env
            .claude_config_dir
            .clone()
            .unwrap_or_else(|| env.home.join(".claude"))
            .join("projects"),
        _ => env
            .codex_home
            .clone()
            .unwrap_or_else(|| env.home.join(".codex"))
            .join("sessions"),
    }
    .canonicalize()
    .map_err(|_| unavailable("the provider's local session directory is unavailable"))?;
    let path = locate(&root, provider, &session.value)?;
    let mut file = open(&path, &root)?;
    if provider == "codex" {
        check_codex_id(&mut file, &session.value)?;
    }
    let meta = file
        .metadata()
        .map_err(|_| unavailable("cannot inspect the transcript"))?;
    let cursor = after.map(decode_cursor).transpose()?;
    let mut start = match &cursor {
        Some(c) => {
            if c.provider != provider
                || c.session_id != session.value
                || c.device != meta.dev()
                || c.inode != meta.ino()
                || c.offset > meta.len()
                || c.include_user != include_user
            {
                return Err(stale());
            }
            if c.offset > 0 {
                file.seek(SeekFrom::Start(c.offset - 1))
                    .map_err(|_| stale())?;
                let mut last = [0];
                file.read_exact(&mut last).map_err(|_| stale())?;
                if last[0] != b'\n' {
                    return Err(stale());
                }
            }
            c.offset
        }
        None => meta.len().saturating_sub(MAX_READ_BYTES),
    };
    let starts_inside_record = if cursor.is_none() && start > 0 {
        file.seek(SeekFrom::Start(start - 1))
            .map_err(|_| unavailable("cannot seek the transcript"))?;
        let mut previous = [0];
        file.read_exact(&mut previous)
            .map_err(|_| unavailable("cannot read the transcript"))?;
        previous[0] != b'\n'
    } else {
        false
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|_| unavailable("cannot seek the transcript"))?;
    let mut bytes = Vec::new();
    // Freeze the end at the observed file size, even if the agent appends during this read.
    (&mut file)
        .take((meta.len() - start).min(MAX_READ_BYTES))
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable("cannot read the transcript"))?;
    let skip = if starts_inside_record {
        // Discard only a partial first record; a complete aligned record fits the budget.
        let skip = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .ok_or_else(|| unavailable("a transcript record exceeds the bounded read size"))?;
        if skip == bytes.len() {
            return Err(unavailable(
                "the latest transcript record exceeds the bounded read size",
            ));
        }
        start += skip as u64;
        skip
    } else {
        0
    };
    let bytes = &bytes[skip..];
    let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    if complete == 0 && bytes.len() as u64 == MAX_READ_BYTES {
        return Err(unavailable(
            "a transcript record exceeds the bounded read size",
        ));
    }
    let mut text = Vec::new();
    for line in bytes[..complete]
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: Value = serde_json::from_slice(line)
            .map_err(|_| unavailable("the transcript contains a malformed complete record"))?;
        if let Some(message) = record_text(&record, provider, &session.value, include_user) {
            text.push(message);
        }
    }
    let offset = start + complete as u64;
    let next = Cursor {
        provider: provider.into(),
        session_id: session.value.clone(),
        device: meta.dev(),
        inode: meta.ino(),
        offset,
        include_user,
    };
    let next_cursor =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&next).expect("serializable cursor"));
    Ok(Output {
        provider: provider.into(),
        session_id: session.value.clone(),
        text: text.join("\n"),
        next_cursor,
        more_above: cursor.is_none() && start > 0,
        more_after: meta.len() > start + bytes.len() as u64,
        pending: complete < bytes.len(),
    })
}

fn decode_cursor(encoded: &str) -> Result<Cursor, HerdrError> {
    let invalid = || HerdrError::invalid_params("after is not a valid session cursor");
    if encoded.len() > 2048 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

fn locate(root: &Path, provider: &str, id: &str) -> Result<PathBuf, HerdrError> {
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut found = None;
    let mut visited = 0;
    let name = format!("{id}.jsonl");
    let suffix = format!("-{id}.jsonl");
    while let Some((dir, depth)) = pending.pop() {
        for entry in std::fs::read_dir(dir)
            .map_err(|_| unavailable("cannot search the session directory"))?
        {
            visited += 1;
            if visited > MAX_SCAN_ENTRIES {
                return Err(unavailable("session discovery exceeded its entry limit"));
            }
            let entry =
                entry.map_err(|_| unavailable("cannot inspect a session directory entry"))?;
            let kind = entry
                .file_type()
                .map_err(|_| unavailable("cannot inspect a session directory entry"))?;
            if kind.is_dir() && depth < if provider == "claude" { 1 } else { 3 } {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                let filename = entry.file_name();
                let filename = filename.to_string_lossy();
                if filename == name || (provider == "codex" && filename.ends_with(&suffix)) {
                    if found.is_some() {
                        return Err(unavailable("multiple transcripts match this session ID"));
                    }
                    found = Some(entry.path());
                }
            }
        }
    }
    found.ok_or_else(|| unavailable("no local transcript matches this pane's session ID"))
}

fn open(path: &Path, root: &Path) -> Result<File, HerdrError> {
    const INVALID_FILE: &str = "the transcript must be a regular file within the session directory with no other hard links";
    let (file, meta) =
        verified_file::open(path, root, |_| Ok::<_, Infallible>(())).map_err(|err| match err {
            FileError::Open(_) => unavailable("cannot open the session transcript"),
            FileError::PathUnavailable => {
                unavailable("cannot check the open transcript's location")
            }
            FileError::Metadata(_) => unavailable("cannot inspect the open transcript"),
            FileError::OutsideRoot | FileError::NotFile | FileError::MultipleLinks => {
                unavailable(INVALID_FILE)
            }
            FileError::Policy(never) => match never {},
        })?;
    // Session cursors require a linked transcript; artifact reads also accept an already-open,
    // unlinked file. Keep this session-specific requirement out of shared file verification.
    if meta.nlink() == 0 {
        return Err(unavailable(INVALID_FILE));
    }
    Ok(file)
}

fn check_codex_id(file: &mut File, id: &str) -> Result<(), HerdrError> {
    let mut header = Vec::new();
    BufReader::new(file.take(MAX_HEADER_BYTES))
        .read_until(b'\n', &mut header)
        .map_err(|_| unavailable("cannot read the Codex session header"))?;
    let record: Value = serde_json::from_slice(&header)
        .map_err(|_| unavailable("the Codex session header is incomplete or unsupported"))?;
    if record["type"] != "session_meta" || record["payload"]["id"] != id {
        return Err(unavailable(
            "the Codex transcript header does not match the pane's session ID",
        ));
    }
    Ok(())
}

fn record_text(record: &Value, provider: &str, id: &str, include_user: bool) -> Option<String> {
    let message = if provider == "claude" {
        if !matches!(record["type"].as_str(), Some("assistant") | Some("user"))
            || record["sessionId"] != id
            || record["isSidechain"] == true
        {
            return None;
        }
        &record["message"]
    } else {
        if record["type"] != "response_item" || record["payload"]["type"] != "message" {
            return None;
        }
        &record["payload"]
    };
    let role = message["role"].as_str()?;
    if role != "assistant" && !(include_user && role == "user") {
        return None;
    }
    let text = if let Some(text) = message["content"].as_str() {
        text.to_owned()
    } else {
        message["content"]
            .as_array()?
            .iter()
            .filter_map(|block| match (provider, role, block["type"].as_str()?) {
                ("claude", _, "text")
                | ("codex", "assistant", "output_text")
                | ("codex", "user", "input_text") => block["text"].as_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    (!text.is_empty()).then_some(if include_user {
        format!("{role}: {text}")
    } else {
        text
    })
}
