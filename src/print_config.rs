use std::path::Path;

use crate::cli::ToolMode;

/// Copy-paste setup for an MCP client that is not started by the installer. Names only the shim.
pub fn render(shim: &Path, mode: ToolMode) -> String {
    let shim = shim.display();
    let tools = mode.as_str();
    format!(
        r#"# herdr-mcp client setup (tools: {tools})
# `herdr-mcp install` creates the shim. Run it first, or point `command` at any herdr-mcp binary.

# Claude Code
claude mcp add --scope user herdr -- {shim} --client claude --tools {tools}

# Claude Code, project .mcp.json
{{"mcpServers": {{"herdr": {{"command": "{shim}", "args": ["--client", "claude", "--tools", "{tools}"]}}}}}}

# Codex
codex mcp add herdr -- {shim} --client codex --tools {tools}

# Codex, $CODEX_HOME/config.toml
[mcp_servers.herdr]
command = "{shim}"
args = ["--client", "codex", "--tools", "{tools}"]
tool_timeout_sec = 330
"#
    )
}
