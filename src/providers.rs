use serde_json::json;

use crate::cli::ToolMode;
use crate::herdr::{AgentInfo, AgentStatus};
use crate::install::codex;
use crate::runtime_mode::Level;

pub struct Provider {
    pub kind: &'static str,
    pub interrupt_key: &'static str,
    pub interrupt_note: Option<&'static str>,
    /// Codex under older detection rules settles to `unknown` while ready.
    pub ready_at_unknown: bool,
    /// Per-launch MCP arguments for the herdr server, given the binary and the tool mode.
    pub mcp_args: Option<fn(bin: &str, mode: ToolMode) -> Vec<String>>,
    /// Per-launch arguments that change only the tool mode of an existing user-scope entry,
    /// for kinds whose overrides merge over it.
    pub mode_args: Option<fn(mode: ToolMode) -> Vec<String>>,
    /// Per-launch arguments that fix the agent's permissions.
    pub runtime_args: Option<fn(level: Level) -> Vec<String>>,
    /// Startup dialogs that Herdr does not report as `blocked`.
    pub dialogs: &'static [Dialog],
}

/// A startup dialog identified from the visible screen.
#[derive(Debug)]
pub struct Dialog {
    pub id: &'static str,
    /// Every one must appear on screen, compared case-insensitively.
    pub all: &'static [&'static str],
    /// At least one must appear when non-empty.
    pub any: &'static [&'static str],
    /// The conservative choice: grants nothing.
    pub skip_keys: &'static [&'static str],
    /// The permissive choice. Only on explicit request.
    pub trust_keys: &'static [&'static str],
    /// Whether the screen text and keys were checked against the real agent.
    pub verified: bool,
}

impl Dialog {
    fn matches(&self, screen: &str) -> bool {
        let screen = screen.to_lowercase();
        self.all.iter().all(|needle| screen.contains(needle))
            && (self.any.is_empty() || self.any.iter().any(|needle| screen.contains(needle)))
    }
}

const CODEX_DIALOGS: &[Dialog] = &[
    Dialog {
        id: "hook_review",
        all: &["hooks need review", "continue without trusting"],
        any: &[],
        skip_keys: &["esc"],
        // The digit may only move the highlight, so Enter confirms it.
        trust_keys: &["2", "enter"],
        verified: true,
    },
    Dialog {
        id: "directory_trust",
        all: &["trust"],
        any: &["this folder", "this directory"],
        skip_keys: &["esc"],
        trust_keys: &["1", "enter"],
        verified: false,
    },
];

const CLAUDE_DIALOGS: &[Dialog] = &[Dialog {
    id: "folder_trust",
    all: &["do you trust the files in this folder"],
    any: &[],
    skip_keys: &["esc"],
    // "Yes, proceed" is the default choice.
    trust_keys: &["enter"],
    verified: false,
}];

/// The first dialog of `kind` that `screen` shows.
pub fn match_dialog(kind: Option<&str>, screen: &str) -> Option<&'static Dialog> {
    provider(kind).dialogs.iter().find(|d| d.matches(screen))
}

pub const PROCESS_INTERRUPT_KEY: &str = "ctrl+c";

const UNVERIFIED: Option<&str> =
    Some("interrupt key not verified for this agent kind; Esc assumed");

const FALLBACK: Provider = Provider {
    kind: "",
    interrupt_key: "esc",
    interrupt_note: UNVERIFIED,
    ready_at_unknown: false,
    mcp_args: None,
    mode_args: None,
    runtime_args: None,
    dialogs: &[],
};

const PROVIDERS: &[Provider] = &[
    Provider {
        kind: "claude",
        interrupt_key: "esc",
        interrupt_note: None,
        ready_at_unknown: false,
        mcp_args: Some(claude_mcp_args),
        mode_args: None,
        runtime_args: Some(claude_runtime_args),
        dialogs: CLAUDE_DIALOGS,
    },
    Provider {
        kind: "codex",
        interrupt_key: "esc",
        interrupt_note: None,
        ready_at_unknown: true,
        mcp_args: Some(codex_mcp_args),
        mode_args: Some(codex_mode_args),
        runtime_args: Some(codex_runtime_args),
        dialogs: CODEX_DIALOGS,
    },
];

pub fn provider(kind: Option<&str>) -> &'static Provider {
    kind.and_then(|kind| PROVIDERS.iter().find(|p| p.kind == kind))
        .unwrap_or(&FALLBACK)
}

/// Ready at `idle` or `done` with `interactive_ready`, or `idle` or `done` once the launch
/// is no longer pending, or, for Codex, `unknown` with `interactive_ready`.
pub fn is_ready(kind: Option<&str>, agent: &AgentInfo) -> bool {
    match agent.agent_status {
        AgentStatus::Idle | AgentStatus::Done => agent.interactive_ready || !agent.launch_pending,
        AgentStatus::Unknown => provider(kind).ready_at_unknown && agent.interactive_ready,
        AgentStatus::Working | AgentStatus::Blocked => false,
    }
}

/// Per-launch MCP arguments for `kind`, or `None` when the kind cannot take them.
pub fn mcp_args(kind: &str, bin: &str, mode: ToolMode) -> Option<Vec<String>> {
    provider(Some(kind)).mcp_args.map(|build| build(bin, mode))
}

/// Per-launch arguments that set only the tool mode of the user-scope entry, or `None` when
/// `kind` cannot override it that way.
pub fn mode_args(kind: &str, mode: ToolMode) -> Option<Vec<String>> {
    provider(Some(kind)).mode_args.map(|build| build(mode))
}

/// Per-launch permission arguments for `kind` at `level`, or `None` when the kind has none.
pub fn runtime_args(kind: &str, level: Level) -> Option<Vec<String>> {
    provider(Some(kind)).runtime_args.map(|build| build(level))
}

pub(crate) fn server_args(client: &str, mode: ToolMode) -> [&str; 4] {
    ["--client", client, "--tools", mode.as_str()]
}

fn claude_mcp_args(bin: &str, mode: ToolMode) -> Vec<String> {
    let config = json!({"mcpServers": {"herdr": {
        "command": bin, "args": server_args("claude", mode)}}});
    vec!["--mcp-config".into(), config.to_string()]
}

fn codex_mode_args(mode: ToolMode) -> Vec<String> {
    vec![
        "-c".into(),
        format!(
            "mcp_servers.herdr.args={}",
            json!(server_args("codex", mode))
        ),
    ]
}

fn codex_mcp_args(bin: &str, mode: ToolMode) -> Vec<String> {
    let overrides = [
        format!("mcp_servers.herdr.command={}", json!(bin)),
        format!(
            "mcp_servers.herdr.args={}",
            json!(server_args("codex", mode))
        ),
        format!("mcp_servers.herdr.env_vars={}", json!(codex::ENV_VARS)),
        format!(
            "mcp_servers.herdr.tool_timeout_sec={}",
            codex::TOOL_TIMEOUT_SEC
        ),
    ];
    overrides
        .into_iter()
        .flat_map(|value| ["-c".to_string(), value])
        .collect()
}

fn codex_runtime_args(level: Level) -> Vec<String> {
    let (approval, sandbox) = match level {
        Level::ApprovalRequired => ("on-request", "read-only"),
        Level::AutoAcceptEdits => ("on-request", "workspace-write"),
        Level::FullAccess => ("never", "danger-full-access"),
    };
    ["-a", approval, "-s", sandbox].map(String::from).to_vec()
}

fn claude_runtime_args(level: Level) -> Vec<String> {
    let mode = match level {
        Level::ApprovalRequired => "manual",
        Level::AutoAcceptEdits => "acceptEdits",
        Level::FullAccess => "bypassPermissions",
    };
    vec!["--permission-mode".into(), mode.into()]
}
