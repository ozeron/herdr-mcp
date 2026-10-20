use std::time::Duration;

use serde_json::{Value, json};

use crate::caller::resolve_caller;
use crate::herdr::HerdrError;
use crate::install::status;
use crate::server::Inner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpStatus {
    Global,
    Injected,
    Injectable,
    Unsupported,
}

impl McpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Injected => "injected",
            Self::Injectable => "injectable",
            Self::Unsupported => "unsupported",
        }
    }
}

fn mcp_status(inner: &Inner, kind: &str) -> McpStatus {
    match &inner.install_env {
        Some(env) => status::mcp_status(env, kind),
        None => match kind {
            "claude" | "codex" => McpStatus::Injectable,
            _ => McpStatus::Unsupported,
        },
    }
}

pub async fn run(inner: &Inner, client_info_name: Option<&str>) -> Result<Value, HerdrError> {
    let client = inner.client()?;
    let compat = client.check_compat().await?;
    let resolution = resolve_caller(&client, &inner.caller_env, inner.ancestry.as_ref()).await?;
    let manifests = client.agent_manifests().await?;

    let agents: Vec<Value> = manifests
        .iter()
        .map(|manifest| {
            json!({
                "kind": manifest.agent,
                "mcp": mcp_status(inner, &manifest.agent).as_str(),
                "active_version": manifest.active_version,
            })
        })
        .collect();
    let caller = resolution.caller.as_ref().map(|caller| {
        json!({
            "pane_id": caller.pane_id,
            "tab_id": caller.tab_id,
            "workspace_id": caller.workspace_id,
        })
    });
    let wait_cap: Duration = inner.profile.wait_cap(client_info_name);

    Ok(json!({
        "caller": caller,
        "caller_verified": resolution.caller_verified,
        "guard_target": resolution.guard_target,
        "tool_mode": inner.mode.as_str(),
        "session": {"socket_path": inner.socket_path},
        "herdr": {"version": compat.version, "protocol": compat.protocol},
        "agents": agents,
        "wait_cap_ms": u64::try_from(wait_cap.as_millis()).unwrap_or(u64::MAX),
    }))
}
