use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorCode {
    AgentNotRunning,
    AgentBlocked,
    AgentPromptStalled,
    PaneNotFound,
    InvalidRequest,
    EventsLost,
    Other(String),
}

impl ApiErrorCode {
    pub fn parse(code: &str) -> Self {
        match code {
            "agent_not_running" => Self::AgentNotRunning,
            "agent_blocked" => Self::AgentBlocked,
            "agent_prompt_stalled" => Self::AgentPromptStalled,
            "pane_not_found" => Self::PaneNotFound,
            "invalid_request" => Self::InvalidRequest,
            "events_lost" => Self::EventsLost,
            other => Self::Other(other.into()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::AgentNotRunning => "agent_not_running",
            Self::AgentBlocked => "agent_blocked",
            Self::AgentPromptStalled => "agent_prompt_stalled",
            Self::PaneNotFound => "pane_not_found",
            Self::InvalidRequest => "invalid_request",
            Self::EventsLost => "events_lost",
            Self::Other(code) => code,
        }
    }
}

#[derive(Debug, Error)]
pub enum HerdrError {
    #[error("herdr is not running (no usable socket at {})", path.display())]
    NotRunning { path: PathBuf },
    #[error(
        "herdr socket path is too long ({len} bytes, limit {max}): {}",
        path.display()
    )]
    SocketPathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    #[error("herdr is incompatible with this bridge: {reason}")]
    Incompatible { reason: String },
    #[error("herdr error {}: {message}", code.as_str())]
    Api { code: ApiErrorCode, message: String },
    #[error("herdr response line exceeded {limit} bytes")]
    LineTooLong { limit: usize },
    #[error("herdr protocol error: {reason}")]
    Protocol { reason: String },
    #[error("herdr socket i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{message}")]
    Tool { code: &'static str, message: String },
}

impl HerdrError {
    pub fn code(&self) -> &str {
        match self {
            Self::NotRunning { .. } => "herdr_not_running",
            Self::SocketPathTooLong { .. } => "herdr_socket_path_too_long",
            Self::Incompatible { .. } => "herdr_incompatible",
            Self::Api { code, .. } => code.as_str(),
            Self::LineTooLong { .. } => "herdr_line_too_long",
            Self::Protocol { .. } => "herdr_protocol_error",
            Self::Io(_) => "herdr_io_error",
            Self::Tool { code, .. } => code,
        }
    }

    pub fn tool(code: &'static str, message: impl Into<String>) -> Self {
        Self::Tool {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn incompatible(reason: impl Into<String>) -> Self {
        Self::Incompatible {
            reason: reason.into(),
        }
    }

    pub(crate) fn protocol(reason: impl Into<String>) -> Self {
        Self::Protocol {
            reason: reason.into(),
        }
    }
}
