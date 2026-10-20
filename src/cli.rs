use clap::{Parser, ValueEnum};

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
