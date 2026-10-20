use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::herdr::{AgentInfo, AgentStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum TurnFrom {
    Working,
    Blocked,
}

/// Opaque handle for one agent turn, handed back to `herdr_wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct Turn {
    pub pane: String,
    pub terminal_id: String,
    #[serde(default)]
    pub shell_pid: Option<u32>,
    pub after: u64,
    pub from: TurnFrom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Stale,
    Matched { superseded: bool },
    Rebase(Turn),
    Pending,
}

impl Turn {
    /// A token for the activity in `agent`, or `None` when it is neither working nor blocked.
    pub fn from_agent(agent: &AgentInfo, shell_pid: Option<u32>) -> Option<Turn> {
        let from = match agent.agent_status {
            AgentStatus::Working => TurnFrom::Working,
            AgentStatus::Blocked => TurnFrom::Blocked,
            _ => return None,
        };
        Some(Turn {
            pane: agent.pane_id.clone(),
            terminal_id: agent.terminal_id.clone(),
            shell_pid,
            after: agent.state_change_seq,
            from,
        })
    }

    pub fn rebased(&self, after: u64) -> Turn {
        Turn {
            after,
            from: TurnFrom::Working,
            ..self.clone()
        }
    }
}

/// Judges one `agent.get` snapshot.
///
/// `shell_pid` is the pane's current shell, when it could be read.
pub fn evaluate(turn: &Turn, agent: &AgentInfo, shell_pid: Option<u32>) -> Verdict {
    let shell_changed = matches!((turn.shell_pid, shell_pid), (Some(was), Some(now)) if was != now);
    if agent.terminal_id != turn.terminal_id || shell_changed {
        return Verdict::Stale;
    }
    let advanced = agent.state_change_seq > turn.after;
    if agent.completion_seq.is_some_and(|seq| seq > turn.after) {
        return Verdict::Matched { superseded: false };
    }
    if advanced && status_settled(agent.agent_status) {
        return Verdict::Matched { superseded: false };
    }
    if advanced && agent.agent_status == AgentStatus::Working {
        return match turn.from {
            TurnFrom::Working => Verdict::Matched { superseded: true },
            TurnFrom::Blocked => Verdict::Rebase(turn.rebased(agent.state_change_seq)),
        };
    }
    Verdict::Pending
}

/// Statuses that end the activity phase: the agent stopped, or needs someone.
fn status_settled(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Idle | AgentStatus::Done | AgentStatus::Blocked | AgentStatus::Unknown
    )
}
