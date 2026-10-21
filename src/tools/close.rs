use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::guard::TargetKind;
use crate::herdr::HerdrError;
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CloseParams {
    /// Pane ID, tab ID or agent name.
    pub target: String,
    /// Allow closing your own pane or its tab. Only when the user asked for it.
    pub allow_self: Option<bool>,
    /// Allow closing a pane no bridge launched. Only when the user asked for it.
    pub force: Option<bool>,
}

pub async fn run(inner: &Inner, params: CloseParams) -> Result<Value, HerdrError> {
    let mut scope = Scope::open(inner)?;
    let request = TargetRequest::new(
        &params.target,
        &[TargetKind::Pane, TargetKind::Tab],
        "herdr_close closes a pane or a tab, not a workspace",
    )
    .allow_self(params.allow_self.unwrap_or(false))
    .provenance(params.force.unwrap_or(false));
    let scoped = scope.target(request).await?;
    let client = scope.client();
    let target = scoped.target.expect("target is required");

    let kind = match target.kind {
        TargetKind::Pane => {
            client.pane_close(&target.id).await?;
            "pane"
        }
        TargetKind::Tab => {
            client.tab_close(&target.id).await?;
            "tab"
        }
        TargetKind::Workspace => unreachable!("workspace targets are refused above"),
    };
    Ok(json!({"closed": true, "kind": kind, "id": target.id}))
}
