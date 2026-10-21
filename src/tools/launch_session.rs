use std::future::Future;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::herdr::{ApiErrorCode, Client, HerdrError};
use crate::place::Placed;
use crate::tools::send::prompt_error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Placed,
    Started,
    Prompted,
    Waiting,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Placed => "placed",
            Self::Started => "started",
            Self::Prompted => "prompted",
            Self::Waiting => "waiting",
        }
    }

    fn cancel_message(self) -> &'static str {
        match self {
            Self::Placed => {
                "the client cancelled the launch before the agent started. The pane is kept: read it, then continue with herdr_send or close it"
            }
            Self::Started => {
                "the client cancelled the launch while the agent was starting. The pane is kept: read it, then continue with herdr_send or close it"
            }
            Self::Prompted | Self::Waiting => {
                "the client cancelled the wait; the agent keeps working. Call herdr_wait with the turn to resume"
            }
        }
    }
}

/// The pane a call placed, with the phase it has reached. Failures in the launch steps leave
/// through one of its exits, so what is cleaned up and what a failure reports live here. Errors
/// from `handle_dialogs` and `take_reply` propagate as they are, and the pane is kept.
pub struct LaunchSession<'a> {
    client: &'a Client,
    placed: Placed,
    name: String,
    phase: Phase,
    turn: Option<Value>,
}

impl<'a> LaunchSession<'a> {
    pub fn new(client: &'a Client, placed: Placed, name: &str) -> Self {
        Self {
            client,
            placed,
            name: name.to_string(),
            phase: Phase::Placed,
            turn: None,
        }
    }

    pub fn client(&self) -> &'a Client {
        self.client
    }

    pub fn placed(&self) -> &Placed {
        &self.placed
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn advance(&mut self, phase: Phase) {
        self.phase = phase;
    }

    /// Reports `turn` with every later failure.
    pub fn attach_turn(&mut self, turn: Value) {
        self.turn = Some(turn);
    }

    /// What this call made, reported with every phase so a timeout or cancel never leads to a repeat.
    pub fn details(&self) -> Value {
        let mut details = self.details_at(self.phase);
        if let Some(turn) = &self.turn {
            details["turn"] = turn.clone();
        }
        details
    }

    fn details_at(&self, phase: Phase) -> Value {
        let mut out = json!({
            "phase": phase.as_str(), "name": self.name, "pane": self.placed.pane_id,
            "tab": self.placed.tab_id, "workspace": self.placed.workspace_id,
            "cwd": self.placed.cwd, "created": self.placed.created_json(),
        });
        if let Some(warning) = &self.placed.warning {
            out["warning"] = json!(warning);
        }
        out
    }

    /// Closes what this call created and returns `err` as it is.
    pub async fn fail_and_clean_up(&self, err: HerdrError) -> HerdrError {
        self.placed.cleanup(self.client).await;
        err
    }

    /// Keeps the pane. A tool error gains the phase details; an API error passes through.
    pub fn fail_keep(&self, err: HerdrError) -> HerdrError {
        match err {
            HerdrError::Tool { code, message } => {
                HerdrError::detailed(code, message, self.details())
            }
            other => other,
        }
    }

    /// Keeps the pane after a refused prompt. A blocked agent reports `started`, a stalled one
    /// `prompted`, and any other error passes through without details.
    pub fn fail_prompt(&self, err: HerdrError) -> HerdrError {
        let phase = match &err {
            HerdrError::Api {
                code: ApiErrorCode::AgentBlocked,
                ..
            } => Phase::Started,
            _ => Phase::Prompted,
        };
        prompt_error(err, &self.placed.pane_id, self.details_at(phase))
    }

    /// Runs `future` unless the client cancels first. A cancel keeps the pane and reports the
    /// current phase.
    pub async fn race<T>(
        &self,
        cancel: &CancellationToken,
        future: impl Future<Output = T>,
    ) -> Result<T, HerdrError> {
        tokio::select! {
            value = future => Ok(value),
            () = cancel.cancelled() => Err(self.cancelled()),
        }
    }

    fn cancelled(&self) -> HerdrError {
        HerdrError::detailed("cancelled", self.phase.cancel_message(), self.details())
    }
}
