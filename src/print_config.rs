use std::path::Path;

use crate::cli::ToolMode;
use crate::install::codex::{ENV_VARS, TOOL_TIMEOUT_SEC};
use crate::providers::server_args;

/// Copy-paste setup for an MCP client that is not started by the installer. Names only the shim.
pub fn render(shim: &Path, mode: ToolMode) -> String {
    let shim = shim.display();
    let tools = mode.as_str();
    let claude_args = server_args("claude", mode);
    let codex_args = server_args("codex", mode);
    let claude_flags = claude_args.join(" ");
    let codex_flags = codex_args.join(" ");
    let quoted = |args: [&str; 4]| args.map(|arg| format!("\"{arg}\"")).join(", ");
    let claude_json = quoted(claude_args);
    let codex_toml = quoted(codex_args);
    let codex_env_vars = ENV_VARS.map(|name| format!("\"{name}\"")).join(", ");
    format!(
        r#"# herdr-mcp client setup (tools: {tools})
# `herdr-mcp install` creates the shim. Run it first, or point `command` at any herdr-mcp binary.

# Claude Code
claude mcp add --scope user herdr -- {shim} {claude_flags}

# Claude Code, project .mcp.json
{{"mcpServers": {{"herdr": {{"command": "{shim}", "args": [{claude_json}]}}}}}}

# Codex
codex mcp add herdr -- {shim} {codex_flags}
# `codex mcp add` cannot set env_vars or tool_timeout_sec. Add them by hand (see the TOML below), or run `herdr-mcp install`.

# Codex, $CODEX_HOME/config.toml
[mcp_servers.herdr]
command = "{shim}"
args = [{codex_toml}]
env_vars = [{codex_env_vars}]
tool_timeout_sec = {TOOL_TIMEOUT_SEC}
"#
    )
}
