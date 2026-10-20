use std::collections::HashMap;

use crate::caller::Resolution;
use crate::herdr::{Client, HerdrError};

pub const SOURCE: &str = "herdr-mcp";
pub const PARENT_TOKEN: &str = "herdr_mcp_parent";
/// Who started an agent in the pane. Kept apart from `PARENT_TOKEN`, which says who created the
/// pane and so who may close it: a launch into an existing pane must not claim the pane.
pub const AGENT_PARENT_TOKEN: &str = "herdr_mcp_agent_parent";
pub const EXTERNAL: &str = "external";

/// The value to record for a pane the caller launches: its verified pane, else `external`.
/// An unverified env-named pane is not recorded, so it can never own children.
pub fn parent_value(resolution: &Resolution) -> &str {
    match (&resolution.caller, resolution.caller_verified) {
        (Some(caller), true) => &caller.pane_id,
        _ => EXTERNAL,
    }
}

/// Herdr stores the token on the pane and returns it as `tokens` on `pane.get`, `session.snapshot`
/// and `agent.get`/`agent.list`, for agent and non-agent panes alike.
pub async fn tag(client: &Client, pane_id: &str, parent: &str) -> Result<(), HerdrError> {
    client
        .pane_report_metadata(pane_id, SOURCE, &[(PARENT_TOKEN, parent)])
        .await
}

/// Records who started the agent in the pane.
pub async fn tag_agent(client: &Client, pane_id: &str, parent: &str) -> Result<(), HerdrError> {
    client
        .pane_report_metadata(pane_id, SOURCE, &[(AGENT_PARENT_TOKEN, parent)])
        .await
}

/// Who may answer the agent's prompts: whoever started it, else whoever created its pane.
pub fn answer_parent_of(tokens: &HashMap<String, String>) -> Option<&str> {
    tokens
        .get(AGENT_PARENT_TOKEN)
        .or_else(|| tokens.get(PARENT_TOKEN))
        .map(String::as_str)
}

/// Who launched the pane, if a bridge did.
pub fn parent_of(tokens: &HashMap<String, String>) -> Option<&str> {
    tokens.get(PARENT_TOKEN).map(String::as_str)
}
