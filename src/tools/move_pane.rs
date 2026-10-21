use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::guard::TargetKind;
use crate::herdr::HerdrError;
use crate::place;
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveParams {
    /// Pane ID or agent name to move.
    pub target: String,
    /// Destination workspace ID or exact label, from herdr_list. The pane moves into a new tab there.
    pub workspace_id: String,
    /// Label for the new tab.
    pub label: Option<String>,
    /// Allow moving your own pane. Only when the user asked for it.
    pub allow_self: Option<bool>,
    /// Allow moving a pane no bridge launched. Only when the user asked for it.
    pub force: Option<bool>,
}

pub async fn run(inner: &Inner, params: MoveParams) -> Result<Value, HerdrError> {
    let mut scope = Scope::open(inner)?;
    let request = TargetRequest::new(
        &params.target,
        &[TargetKind::Pane],
        "herdr_move takes a pane ID or an agent name",
    )
    .allow_self(params.allow_self.unwrap_or(false))
    .provenance(params.force.unwrap_or(false));
    let scoped = scope.target(request).await?;
    let client = scope.client();
    let pane = scoped.target.id.clone();
    let dest = place::destination(client, Some(&params.workspace_id), None, None).await?;

    let moved = client
        .pane_move_to_new_tab(&pane, &dest.workspace_id, params.label.as_deref())
        .await?;
    let mut out = json!({
        "moved": moved.changed,
        "previous": {
            "pane": moved.previous_pane_id,
            "tab": moved.previous_tab_id,
            "workspace": moved.previous_workspace_id,
        },
        "pane": moved.pane.pane_id,
        "tab": moved.pane.tab_id,
        "workspace": moved.pane.workspace_id,
        "cwd": moved.pane.cwd,
    });
    if let Some(reason) = &moved.reason {
        out["reason"] = json!(reason);
    }
    if moved.changed && moved.pane.pane_id != moved.previous_pane_id {
        out["note"] = json!(
            "the pane ID changed with its workspace; the process kept running. Use the new pane ID or the agent name from now on"
        );
    }
    if let Some(closed) = &moved.closed_workspace_id {
        out["closed_workspace"] = json!(closed);
    }
    Ok(out)
}
