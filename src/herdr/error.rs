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
    AgentNotFound,
    AgentPaneBusy,
    AgentNotIdle,
    Timeout,
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
            "agent_not_found" => Self::AgentNotFound,
            "agent_pane_busy" => Self::AgentPaneBusy,
            "agent_not_idle" => Self::AgentNotIdle,
            "timeout" => Self::Timeout,
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
            Self::AgentNotFound => "agent_not_found",
            Self::AgentPaneBusy => "agent_pane_busy",
            Self::AgentNotIdle => "agent_not_idle",
            Self::Timeout => "timeout",
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
    #[error("{message}")]
    Detailed {
        code: &'static str,
        message: String,
        details: serde_json::Value,
    },
}

impl HerdrError {
    /// The pane, agent or request no longer resolves: Herdr answers a vanished target with any of these.
    pub fn is_target_gone(&self) -> bool {
        matches!(
            self,
            Self::Api {
                code: ApiErrorCode::PaneNotFound
                    | ApiErrorCode::InvalidRequest
                    | ApiErrorCode::AgentNotRunning
                    | ApiErrorCode::AgentNotFound,
                ..
            }
        )
    }

    pub fn code(&self) -> &str {
        match self {
            Self::NotRunning { .. } => "herdr_not_running",
            Self::SocketPathTooLong { .. } => "herdr_socket_path_too_long",
            Self::Incompatible { .. } => "herdr_incompatible",
            Self::Api { code, .. } => code.as_str(),
            Self::LineTooLong { .. } => "herdr_line_too_long",
            Self::Protocol { .. } => "herdr_protocol_error",
            Self::Io(_) => "herdr_io_error",
            Self::Tool { code, .. } | Self::Detailed { code, .. } => code,
        }
    }

    /// Fields to report beside `code` and `message`, such as the step a multi-step tool reached.
    pub fn details(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Detailed { details, .. } => Some(details),
            _ => None,
        }
    }

    pub fn detailed(
        code: &'static str,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self::Detailed {
            code,
            message: message.into(),
            details,
        }
    }

    pub fn tool(code: &'static str, message: impl Into<String>) -> Self {
        Self::Tool {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::tool("invalid_params", message)
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
