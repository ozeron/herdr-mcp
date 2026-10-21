use std::fmt;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;

use crate::herdr::{HerdrError, PaneProcessInfo, PaneProcessInfoProcess};

/// How much a launched agent may do without asking, adapted from T3 Code's runtime modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    /// Add no flags: the agent keeps whatever its own config allows. Default.
    #[default]
    Inherit,
    /// Asks before edits and commands.
    ApprovalRequired,
    /// Edits in the workspace without asking; commands still ask.
    AutoAcceptEdits,
    /// No approvals and no sandbox. Only when the user asked.
    FullAccess,
}

/// A mode that restricts: the ones that can be ranked against the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    ApprovalRequired,
    AutoAcceptEdits,
    FullAccess,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalRequired => "approval-required",
            Self::AutoAcceptEdits => "auto-accept-edits",
            Self::FullAccess => "full-access",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RuntimeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::ApprovalRequired => "approval-required",
            Self::AutoAcceptEdits => "auto-accept-edits",
            Self::FullAccess => "full-access",
        }
    }

    pub fn level(self) -> Option<Level> {
        match self {
            Self::Inherit => None,
            Self::ApprovalRequired => Some(Level::ApprovalRequired),
            Self::AutoAcceptEdits => Some(Level::AutoAcceptEdits),
            Self::FullAccess => Some(Level::FullAccess),
        }
    }
}

/// `known: <mode>` or `unknown`, as reported in the launch result.
pub fn describe(caller: Option<Level>) -> String {
    match caller {
        Some(level) => format!("known: {level}"),
        None => "unknown".into(),
    }
}

/// Refuses a request above the caller's known mode. With an unknown caller, allows up to
/// auto-accept-edits, and full-access only with `allow_full_access`.
pub fn check_escalation(
    requested: RuntimeMode,
    caller: Option<Level>,
    allow_full_access: bool,
) -> Result<(), HerdrError> {
    let Some(requested) = requested.level() else {
        return Ok(());
    };
    let allowed = match caller {
        Some(known) => requested <= known,
        None => requested < Level::FullAccess || allow_full_access,
    };
    if allowed {
        return Ok(());
    }
    let message = match caller {
        Some(known) => format!(
            "the caller runs in {known}, so it cannot launch a child in {requested}"
        ),
        None => "the caller's mode is unknown, so full-access needs allow_full_access: true, and only when the user asked for it".into(),
    };
    Err(HerdrError::detailed(
        "runtime_mode_escalation_denied",
        message,
        serde_json::json!({
            "runtime_mode": requested.as_str(),
            "caller_runtime_mode": describe(caller),
        }),
    ))
}

/// The caller's effective mode from its foreground codex or claude argv, or `None` when no flag
/// says. Config files are invisible here, so `None` is honest.
pub fn caller_level(info: &PaneProcessInfo) -> Option<Level> {
    info.foreground_processes.iter().find_map(|process| {
        let argv = argv_of(process);
        match agent_of(process, &argv)? {
            "codex" => codex_level(&argv),
            _ => claude_level(&argv),
        }
    })
}

fn argv_of(process: &PaneProcessInfoProcess) -> Vec<String> {
    match (&process.argv, &process.cmdline) {
        (Some(argv), _) => argv.clone(),
        (None, Some(cmdline)) => cmdline.split_whitespace().map(String::from).collect(),
        (None, None) => Vec::new(),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Codex and Claude may run under node, so the script argument counts too.
fn agent_of(process: &PaneProcessInfoProcess, argv: &[String]) -> Option<&'static str> {
    let names = std::iter::once(process.name.as_str())
        .chain(process.argv0.as_deref())
        .chain(argv.iter().take(2).map(String::as_str));
    names.map(basename).find_map(|name| match name {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        _ => None,
    })
}

/// The value of `short` or `long`, as `-a v`, `--long v` or `--long=v`.
fn flag_value<'a>(argv: &'a [String], short: &str, long: &str) -> Option<&'a str> {
    let mut args = argv.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == short || arg == long {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix(&format!("{long}=")) {
            return Some(value);
        }
    }
    None
}

fn codex_level(argv: &[String]) -> Option<Level> {
    let has = |flag: &str| argv.iter().any(|arg| arg == flag);
    if has("--dangerously-bypass-approvals-and-sandbox") || has("--yolo") {
        return Some(Level::FullAccess);
    }
    let approval = flag_value(argv, "-a", "--ask-for-approval");
    match flag_value(argv, "-s", "--sandbox")? {
        "danger-full-access" if approval == Some("never") => Some(Level::FullAccess),
        "workspace-write" => Some(Level::AutoAcceptEdits),
        "read-only" => Some(Level::ApprovalRequired),
        _ => None,
    }
}

fn claude_level(argv: &[String]) -> Option<Level> {
    if argv
        .iter()
        .any(|arg| arg == "--dangerously-skip-permissions")
    {
        return Some(Level::FullAccess);
    }
    match flag_value(argv, "--permission-mode", "--permission-mode")? {
        "bypassPermissions" => Some(Level::FullAccess),
        "acceptEdits" => Some(Level::AutoAcceptEdits),
        "default" | "manual" | "plan" => Some(Level::ApprovalRequired),
        _ => None,
    }
}
