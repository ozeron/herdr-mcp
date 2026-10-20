use std::time::Duration;

use clap::ValueEnum;

pub const CODEX_CLIENT_NAME: &str = "codex-mcp-client";
pub const CODEX_WAIT_CAP: Duration = Duration::from_secs(280);
pub const CLAUDE_WAIT_CAP: Duration = Duration::from_secs(600);
pub const DEFAULT_WAIT_CAP: Duration = Duration::from_secs(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClientKind {
    Claude,
    Codex,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    client: Option<ClientKind>,
    max_wait_override: Option<Duration>,
}

impl Profile {
    pub fn new(client: Option<ClientKind>, max_wait_override: Option<Duration>) -> Self {
        Self {
            client,
            max_wait_override,
        }
    }

    /// `--max-wait-ms` wins, then `--client`, then the MCP `clientInfo` name.
    pub fn wait_cap(&self, client_info_name: Option<&str>) -> Duration {
        if let Some(cap) = self.max_wait_override {
            return cap;
        }
        let from_info = (client_info_name == Some(CODEX_CLIENT_NAME)).then_some(ClientKind::Codex);
        match self.client.or(from_info) {
            Some(ClientKind::Codex) => CODEX_WAIT_CAP,
            Some(ClientKind::Claude) => CLAUDE_WAIT_CAP,
            None => DEFAULT_WAIT_CAP,
        }
    }
}
