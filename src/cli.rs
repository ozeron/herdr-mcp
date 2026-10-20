use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

use crate::profile::ClientKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolMode {
    Read,
    Control,
}

impl ToolMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Control => "control",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "herdr-mcp",
    version,
    about = "MCP bridge over Herdr's socket API"
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Herdr session name; overrides HERDR_SOCKET_PATH and HERDR_SESSION
    #[arg(long)]
    pub session: Option<String>,
    /// MCP client this bridge is installed for; sets the wait cap
    #[arg(long, value_enum)]
    pub client: Option<ClientKind>,
    /// Override the wait cap in milliseconds
    #[arg(long)]
    pub max_wait_ms: Option<u64>,
    /// Tools to register
    #[arg(long, value_enum, default_value = "read")]
    pub tools: ToolMode,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add or remove the global Claude and Codex entries behind a stable shim
    Install(InstallArgs),
    /// Print MCP client setup snippets for external Claude and Codex
    PrintConfig(PrintConfigArgs),
}

#[derive(Debug, ClapArgs)]
pub struct PrintConfigArgs {
    /// Tools the printed entries register
    #[arg(long, value_enum, default_value = "read")]
    pub tools: ToolMode,
}

#[derive(Debug, ClapArgs)]
pub struct InstallArgs {
    /// Register the control tools; they hold shell authority over every pane
    #[arg(long, conflicts_with_all = ["status", "remove"])]
    pub control: bool,
    /// Report the install state and write nothing
    #[arg(long, conflicts_with = "remove")]
    pub status: bool,
    /// Remove the entries and shim that point at this bridge
    #[arg(long)]
    pub remove: bool,
    /// Apply without prompting
    #[arg(long)]
    pub yes: bool,
}
