use std::time::Duration;

use serde_json::{Value, json};

use crate::herdr::HerdrError;
use crate::install::status;
use crate::server::Inner;

pub async fn run(inner: &Inner, client_info_name: Option<&str>) -> Result<Value, HerdrError> {
    let (client, active) = inner.session()?;
    // `join!`, not `try_join!`, so an error is chosen by order (compat, caller, manifests) and not
    // by whichever call fails first.
    let (compat, resolution, manifests, sessions) = tokio::join!(
        client.check_compat(),
        inner.resolve_caller(&client, &active),
        client.agent_manifests(),
        inner.sessions.list(),
    );
    let (compat, resolution, manifests) = (compat?, resolution?, manifests?);

    let agents: Vec<Value> = manifests
        .iter()
        .map(|manifest| {
            json!({
                "kind": manifest.agent,
                "mcp": status::mcp_status_for(inner.install_env.as_ref(), &manifest.agent).as_str(),
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

    let mut out = json!({
        "caller": caller,
        "caller_verified": resolution.caller_verified,
        "guard_target": resolution.guard_target,
        "tool_mode": inner.mode.as_str(),
        "session": {"name": active.name, "socket": active.socket_path, "home": active.home},
        "sessions": sessions,
        "herdr": {"version": compat.version, "protocol": compat.protocol},
        "agents": agents,
        "wait_cap_ms": u64::try_from(wait_cap.as_millis()).unwrap_or(u64::MAX),
    });
    if inner.is_remote() {
        // The default inbox; a call can still pick another with `inbox`.
        out["inbox"] = json!(crate::tools::mail::remote_inbox(client_info_name, None)?);
    }
    Ok(out)
}
