use serde_json::json;

use crate::cli::ToolMode;
use crate::herdr::{AgentInfo, AgentStatus};
use crate::install::codex;

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
};

const PROVIDERS: &[Provider] = &[
    Provider {
        kind: "claude",
        interrupt_key: "esc",
        interrupt_note: None,
        ready_at_unknown: false,
        mcp_args: Some(claude_mcp_args),
        mode_args: None,
    },
    Provider {
        kind: "codex",
        interrupt_key: "esc",
        interrupt_note: None,
        ready_at_unknown: true,
        mcp_args: Some(codex_mcp_args),
        mode_args: Some(codex_mode_args),
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
