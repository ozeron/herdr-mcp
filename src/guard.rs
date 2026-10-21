use crate::herdr::{Client, HerdrError, SessionSnapshot};
use crate::provenance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Pane,
    Tab,
    Workspace,
}

/// A target resolved against one snapshot, with every pane it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub kind: TargetKind,
    pub id: String,
    pub pane_ids: Vec<String>,
}

/// Matches a pane, tab or workspace ID, or an agent name, in `snapshot`.
pub fn resolve(snapshot: &SessionSnapshot, target: &str) -> Option<Resolved> {
    let panes_where = |keep: &dyn Fn(&crate::herdr::PaneInfo) -> bool| -> Vec<String> {
        snapshot
            .panes
            .iter()
            .filter(|pane| keep(pane))
            .map(|pane| pane.pane_id.clone())
            .collect()
    };
    if snapshot.panes.iter().any(|pane| pane.pane_id == target) {
        return Some(pane(target));
    }
    if snapshot.tabs.iter().any(|tab| tab.tab_id == target) {
        return Some(Resolved {
            kind: TargetKind::Tab,
            id: target.into(),
            pane_ids: panes_where(&|pane| pane.tab_id == target),
        });
    }
    if snapshot
        .workspaces
        .iter()
        .any(|workspace| workspace.workspace_id == target)
    {
        return Some(Resolved {
            kind: TargetKind::Workspace,
            id: target.into(),
            pane_ids: panes_where(&|pane| pane.workspace_id == target),
        });
    }
    snapshot
        .agents
        .iter()
        .find(|agent| agent.name.as_deref() == Some(target))
        .map(|agent| pane(&agent.pane_id))
}

fn pane(id: &str) -> Resolved {
    Resolved {
        kind: TargetKind::Pane,
        id: id.into(),
        pane_ids: vec![id.into()],
    }
}

/// `resolve`, then Herdr's own alias lookup for a moved pane or an agent not in the snapshot.
pub(crate) async fn resolve_target(
    client: &Client,
    snapshot: &SessionSnapshot,
    target: &str,
) -> Result<Resolved, HerdrError> {
    if let Some(resolved) = resolve(snapshot, target) {
        return Ok(resolved);
    }
    let not_found = || {
        HerdrError::tool(
            "target_not_found",
            format!("no pane, tab or agent matches {target:?}"),
        )
    };
    match client.agent_get(target).await {
        Ok(agent) => resolve(snapshot, &agent.pane_id).ok_or_else(not_found),
        Err(err) if err.is_target_gone() => Err(not_found()),
        Err(err) => Err(err),
    }
}

/// Refuse a target that is, or contains, the pane the guards protect.
/// `guard_pane` is `Resolution::guard_target`, which fails closed for an unverified caller.
pub(crate) fn self_guard(
    guard_pane: Option<&str>,
    target: &Resolved,
    allow_self: bool,
) -> Result<(), HerdrError> {
    let Some(guard_pane) = guard_pane else {
        return Ok(());
    };
    if !allow_self && target.pane_ids.iter().any(|id| id == guard_pane) {
        return Err(HerdrError::tool(
            "self_target",
            format!(
                "{} contains your own pane {guard_pane}; set allow_self only if the user asked for it",
                target.id
            ),
        ));
    }
    Ok(())
}

/// Refuse a target with any pane no bridge launched, unless `force`.
pub(crate) fn provenance_guard(
    snapshot: &SessionSnapshot,
    target: &Resolved,
    force: bool,
) -> Result<(), HerdrError> {
    if force {
        return Ok(());
    }
    let unlaunched: Vec<&str> = target
        .pane_ids
        .iter()
        .filter(|id| {
            !snapshot
                .panes
                .iter()
                .any(|pane| &pane.pane_id == *id && provenance::parent_of(&pane.tokens).is_some())
        })
        .map(String::as_str)
        .collect();
    if unlaunched.is_empty() {
        return Ok(());
    }
    Err(HerdrError::tool(
        "not_launched_by_bridge",
        format!(
            "{} holds panes no bridge launched ({}); set force only if the user asked for it",
            target.id,
            unlaunched.join(", ")
        ),
    ))
}
