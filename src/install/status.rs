use std::path::PathBuf;

use clap::ValueEnum;

use super::shim::{self, ShimState};
use super::{InstallEnv, InstallError, claude, codex};
use crate::cli::ToolMode;
use crate::profile::ClientKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpStatus {
    Global,
    Injected,
    Injectable,
    Unsupported,
}

impl McpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Injected => "injected",
            Self::Injectable => "injectable",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub command: String,
    pub args: Vec<String>,
    pub env_vars: Option<Vec<String>>,
    pub tool_timeout_sec: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Absent,
    Ours,
    Drifted,
    Foreign,
    ShimBroken,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Ours => "ours",
            Self::Drifted => "drifted",
            Self::Foreign => "foreign",
            Self::ShimBroken => "shim_broken",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    EnvVars,
    ToolTimeout,
    ToolsMode,
    Args,
    ShimMoved,
}

impl Drift {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnvVars => "env_vars lost",
            Self::ToolTimeout => "tool_timeout_sec lost",
            Self::ToolsMode => "tools mode differs",
            Self::Args => "args changed",
            Self::ShimMoved => "shim points at another binary",
        }
    }

    pub fn needs_entry_write(self) -> bool {
        self != Self::ShimMoved
    }
}

#[derive(Debug, Clone)]
pub struct Classification {
    pub state: State,
    pub mode: Option<ToolMode>,
    pub drift: Vec<Drift>,
    pub config_path: PathBuf,
    pub warnings: Vec<String>,
}

fn parse_mode(args: &[String]) -> Option<ToolMode> {
    let value = args
        .iter()
        .position(|arg| arg == "--tools")
        .and_then(|at| args.get(at + 1))?;
    ToolMode::from_str(value, false).ok()
}

fn args_match(kind: ClientKind, args: &[String]) -> bool {
    args.len() == 4
        && args[0] == "--client"
        && args[1] == kind.as_str()
        && args[2] == "--tools"
        && parse_mode(args).is_some()
}

/// Classifies the user-scope `herdr` entry. `desired` is the mode an install would write.
pub fn classify(
    env: &InstallEnv,
    kind: ClientKind,
    desired: Option<ToolMode>,
) -> Result<Classification, InstallError> {
    let (entry, config_path, shadows) = match kind {
        ClientKind::Claude => {
            let read = claude::read(env)?;
            (read.entry, claude::config_path(env), read.shadows)
        }
        ClientKind::Codex => (codex::read(env)?, codex::config_path(env), Vec::new()),
    };
    let warnings = shadows
        .into_iter()
        .map(|dir| {
            format!("a project-scope `herdr` entry in {dir} shadows the user entry in that project")
        })
        .collect();
    let (state, mode, drift) = match entry {
        None => (State::Absent, None, Vec::new()),
        Some(entry) => classify_entry(env, kind, desired, &entry),
    };
    Ok(Classification {
        state,
        mode,
        drift,
        config_path,
        warnings,
    })
}

fn classify_entry(
    env: &InstallEnv,
    kind: ClientKind,
    desired: Option<ToolMode>,
    entry: &Entry,
) -> (State, Option<ToolMode>, Vec<Drift>) {
    let mode = parse_mode(&entry.args);
    if entry.command != env.shim_path().display().to_string() {
        return (State::Foreign, mode, Vec::new());
    }
    let shim = shim::inspect(env);
    match shim {
        ShimState::Foreign => return (State::Foreign, mode, Vec::new()),
        ShimState::Missing | ShimState::Dangling => {
            return (State::ShimBroken, mode, Vec::new());
        }
        ShimState::Current | ShimState::Moved => {}
    }
    let mut drift = Vec::new();
    if shim == ShimState::Moved {
        drift.push(Drift::ShimMoved);
    }
    if !args_match(kind, &entry.args) {
        drift.push(Drift::Args);
    } else if desired.is_some_and(|wanted| Some(wanted) != mode) {
        drift.push(Drift::ToolsMode);
    }
    if kind == ClientKind::Codex {
        let has_all_vars = entry.env_vars.as_ref().is_some_and(|vars| {
            codex::ENV_VARS
                .iter()
                .all(|name| vars.iter().any(|v| v == name))
        });
        if !has_all_vars {
            drift.push(Drift::EnvVars);
        }
        if entry.tool_timeout_sec != Some(codex::TOOL_TIMEOUT_SEC) {
            drift.push(Drift::ToolTimeout);
        }
    }
    let state = if drift.is_empty() {
        State::Ours
    } else {
        State::Drifted
    };
    (state, mode, drift)
}

/// How the bridge reaches an agent kind: `global` when the user-scope entry is ours
/// (even if drifted), otherwise `injectable` for claude and codex.
pub fn mcp_status(env: &InstallEnv, kind: &str) -> McpStatus {
    let Some(kind) = ClientKind::from_kind(kind) else {
        return McpStatus::Unsupported;
    };
    match classify(env, kind, None) {
        Ok(found) if matches!(found.state, State::Ours | State::Drifted) => McpStatus::Global,
        _ => McpStatus::Injectable,
    }
}

/// The tools mode of the user-scope entry, read with the same classifier as `mcp_status`.
/// An entry whose args carry no readable mode runs in the bridge default, read.
pub fn global_mode(env: &InstallEnv, kind: &str) -> Option<ToolMode> {
    let kind = ClientKind::from_kind(kind)?;
    let found = classify(env, kind, None).ok()?;
    Some(found.mode.unwrap_or(ToolMode::Read))
}

/// `mcp_status`, or `injectable` for claude and codex when there is no install environment.
pub fn mcp_status_for(env: Option<&InstallEnv>, kind: &str) -> McpStatus {
    match env {
        Some(env) => mcp_status(env, kind),
        None => {
            ClientKind::from_kind(kind).map_or(McpStatus::Unsupported, |_| McpStatus::Injectable)
        }
    }
}
