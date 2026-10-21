use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

use crate::profile::ClientKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolMode {
    Read,
    Control,
}

impl ToolMode {
    pub fn from_control(control: bool) -> Self {
        if control { Self::Control } else { Self::Read }
    }

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
    /// Tunnel mode, set by `tunnel run`: audit log and `herdr_tunnel_stop`
    #[arg(long)]
    pub tunnel: bool,
    /// Stop the tunnel at this Unix time (seconds); needs --tunnel
    #[arg(long, requires = "tunnel")]
    pub expires_at: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add or remove the global Claude and Codex entries behind a stable shim
    Install(InstallArgs),
    /// Print MCP client setup snippets for external Claude and Codex
    PrintConfig(PrintConfigArgs),
    /// Reach the read tools from ChatGPT through OpenAI's tunnel-client
    #[command(subcommand)]
    Tunnel(TunnelCommand),
}

#[derive(Debug, Subcommand)]
pub enum TunnelCommand {
    /// Save the tunnel ID and store the runtime key in the macOS Keychain
    Setup,
    /// Run tunnel-client in the foreground; access lasts until it exits
    Run(TunnelRunArgs),
    /// Run the tunnel in the background as a macOS LaunchAgent
    Start(TunnelStartArgs),
    /// Stop the background tunnel and remove its LaunchAgent
    Stop,
    /// Show whether the background tunnel runs, its mode, expiry and recent audit lines
    Status,
    /// Print the tunnel audit log
    Log(TunnelLogArgs),
}

#[derive(Debug, ClapArgs)]
pub struct TunnelRunArgs {
    /// Herdr session name for the bridge
    #[arg(long)]
    pub session: Option<String>,
    /// Register the control tools; `tunnel start --full` sets --full-confirmed after the disclosure
    #[arg(long)]
    pub full: bool,
    /// Proof that `tunnel start --full` showed the disclosure; do not set by hand
    #[arg(long, requires = "full")]
    pub full_confirmed: bool,
    /// Stop the tunnel at this Unix time (seconds)
    #[arg(long)]
    pub expires_at: Option<u64>,
}

#[derive(Debug, ClapArgs)]
pub struct TunnelStartArgs {
    /// Give ChatGPT the control tools; prints a disclosure and asks you to type `full access`
    #[arg(long)]
    pub full: bool,
    /// Stop the tunnel after this long, like 30m, 8h or 2d
    #[arg(long = "for", value_name = "DURATION")]
    pub duration: Option<String>,
    /// Herdr session name for the bridge
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct TunnelLogArgs {
    /// How many of the last lines to print
    #[arg(short = 'n', long, default_value_t = 50)]
    pub lines: usize,
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
    /// Re-point an installed shim at this build; the plugin's build step runs it after updates
    #[arg(long, conflicts_with_all = ["control", "status", "remove"])]
    pub relink_shim: bool,
}
