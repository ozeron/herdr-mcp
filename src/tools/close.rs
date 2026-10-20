use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caller::resolve_caller;
use crate::guard::{self, TargetKind};
use crate::herdr::HerdrError;
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
    let client = inner.client()?;
    let (snapshot, resolution) = tokio::try_join!(
        client.session_snapshot(),
        resolve_caller(&client, &inner.caller_env, inner.ancestry.as_ref()),
    )?;
    let target = guard::resolve_target(&client, &snapshot, &params.target).await?;
    if target.kind == TargetKind::Workspace {
        return Err(HerdrError::tool(
            "unsupported_target",
            "herdr_close closes a pane or a tab, not a workspace",
        ));
    }
    guard::self_guard(
        resolution.guard_target.as_deref(),
        &target,
        params.allow_self.unwrap_or(false),
    )?;
    guard::provenance_guard(&snapshot, &target, params.force.unwrap_or(false))?;

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
