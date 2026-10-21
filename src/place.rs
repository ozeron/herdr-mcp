use std::collections::HashMap;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caller::Resolution;
use crate::guard::TargetKind;
use crate::herdr::{
    Client, HerdrError, PaneSplitParams, SplitDirection, TabCreateParams, WorkspaceCreateParams,
    WorktreeCreateParams,
};
use crate::provenance;
use crate::scope::{Scope, Scoped, TargetRequest};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct WorktreeWhere {
    /// Branch for the new Herdr worktree.
    pub branch: String,
    /// Base ref to branch from.
    pub base: Option<String>,
}

/// Where to put the work. Give at most one field; the default is a new tab.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub struct Where {
    /// An existing idle shell pane (ID or agent name). Refused if its foreground is not the shell.
    pub pane: Option<String>,
    /// Split the pane named by `of`, or the calling pane when `of` is omitted. External callers
    /// must give `of`.
    pub split: Option<SplitDirection>,
    /// Pane to split (pane ID or agent name). Defaults to the calling pane; external callers must give it.
    pub of: Option<String>,
    /// A new tab in the calling pane's workspace (or the focused workspace). This is the default.
    pub tab: Option<bool>,
    /// A new workspace.
    pub workspace: Option<bool>,
    /// A new Herdr worktree and workspace; its root pane is used.
    pub worktree: Option<WorktreeWhere>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    /// Names a new tab, workspace or worktree.
    pub label: Option<String>,
}

/// What this call created, so a failure after placement closes only that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Created {
    Pane(String),
    Tab(String),
    Workspace(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub created: Option<Created>,
}

impl Placed {
    pub fn created_json(&self) -> Value {
        match &self.created {
            Some(Created::Pane(id)) => json!({"kind": "pane", "id": id}),
            Some(Created::Tab(id)) => json!({"kind": "tab", "id": id}),
            Some(Created::Workspace(id)) => json!({"kind": "workspace", "id": id}),
            None => Value::Null,
        }
    }

    /// Closes what `place` created. Best effort: the original error is the one worth reporting.
    pub async fn cleanup(&self, client: &Client) {
        let _ = match &self.created {
            Some(Created::Pane(id)) => client.pane_close(id).await,
            Some(Created::Tab(id)) => client.tab_close(id).await,
            Some(Created::Workspace(id)) => client.workspace_close(id).await,
            None => return,
        };
    }
}

enum Plan<'a> {
    Existing(&'a str),
    Split(SplitDirection),
    Tab,
    Workspace,
    Worktree(&'a WorktreeWhere),
}

fn plan<'a>(where_: Option<&'a Where>, options: &Options) -> Result<Plan<'a>, HerdrError> {
    let Some(where_) = where_ else {
        return Ok(Plan::Tab);
    };
    if where_.of.is_some() && where_.split.is_none() {
        return Err(HerdrError::invalid_params("of only applies to split"));
    }
    let mut chosen = Vec::new();
    if let Some(pane) = &where_.pane {
        chosen.push(Plan::Existing(pane));
    }
    if let Some(direction) = where_.split {
        chosen.push(Plan::Split(direction));
    }
    if where_.tab == Some(true) {
        chosen.push(Plan::Tab);
    }
    if where_.workspace == Some(true) {
        chosen.push(Plan::Workspace);
    }
    if let Some(worktree) = &where_.worktree {
        chosen.push(Plan::Worktree(worktree));
    }
    let plan = match chosen.len() {
        0 => Plan::Tab,
        1 => chosen.remove(0),
        _ => {
            return Err(HerdrError::invalid_params(
                "where takes one of pane, split, tab, workspace or worktree",
            ));
        }
    };
    match plan {
        Plan::Existing(_) if options.cwd.is_some() || !options.env.is_empty() => Err(
            HerdrError::invalid_params("cwd and env cannot be applied to an existing pane"),
        ),
        Plan::Worktree(_) if !options.env.is_empty() => Err(HerdrError::invalid_params(
            "env cannot be applied to a new worktree",
        )),
        Plan::Existing(_) | Plan::Split(_) if options.label.is_some() => Err(
            HerdrError::invalid_params("label names a new tab, workspace or worktree, not a pane"),
        ),
        plan => Ok(plan),
    }
}

/// Rejects a conflicting `where` or options before anything is created.
pub fn validate(where_: Option<&Where>, options: &Options) -> Result<(), HerdrError> {
    plan(where_, options).map(|_| ())
}

/// Resolves `where_` to a pane. New panes are tagged with provenance before they are returned;
/// if tagging fails, what was created is closed.
pub async fn place(
    client: &Client,
    resolution: &Resolution,
    where_: Option<&Where>,
    options: &Options,
) -> Result<Placed, HerdrError> {
    let plan = plan(where_, options)?;
    let verified = resolution
        .caller
        .as_ref()
        .filter(|_| resolution.caller_verified);
    let caller_workspace = verified.and_then(|caller| caller.workspace_id.clone());
    let placed = match plan {
        Plan::Existing(target) => return existing(client, resolution, target).await,
        Plan::Split(direction) => {
            let target = match where_.and_then(|w| w.of.as_deref()) {
                Some(of) => resolve_of(client, of).await?,
                None => verified
                    .map(|caller| caller.pane_id.clone())
                    .ok_or_else(|| {
                        HerdrError::invalid_params(
                            "an external caller must name the pane to split with where.of",
                        )
                    })?,
            };
            let params = PaneSplitParams {
                target_pane_id: Some(target),
                direction,
                cwd: options.cwd.clone(),
                env: options.env.clone(),
                ..PaneSplitParams::default()
            };
            let pane = client.pane_split(&params).await?;
            Placed {
                created: Some(Created::Pane(pane.pane_id.clone())),
                pane_id: pane.pane_id,
                tab_id: pane.tab_id,
                workspace_id: pane.workspace_id,
            }
        }
        Plan::Tab => {
            let params = TabCreateParams {
                workspace_id: caller_workspace,
                cwd: options.cwd.clone(),
                label: options.label.clone(),
                env: options.env.clone(),
                ..TabCreateParams::default()
            };
            let created = client.tab_create(&params).await?;
            Placed {
                created: Some(Created::Tab(created.tab.tab_id.clone())),
                pane_id: created.root_pane.pane_id,
                tab_id: created.tab.tab_id,
                workspace_id: created.tab.workspace_id,
            }
        }
        Plan::Workspace => {
            let params = WorkspaceCreateParams {
                cwd: options.cwd.clone(),
                label: options.label.clone(),
                env: options.env.clone(),
                ..WorkspaceCreateParams::default()
            };
            let created = client.workspace_create(&params).await?;
            Placed {
                created: Some(Created::Workspace(created.workspace.workspace_id.clone())),
                pane_id: created.root_pane.pane_id,
                tab_id: created.tab.tab_id,
                workspace_id: created.workspace.workspace_id,
            }
        }
        Plan::Worktree(worktree) => {
            let params = WorktreeCreateParams {
                workspace_id: caller_workspace,
                cwd: options.cwd.clone(),
                branch: Some(worktree.branch.clone()),
                base: worktree.base.clone(),
                label: options.label.clone(),
                ..WorktreeCreateParams::default()
            };
            let created = client.worktree_create(&params).await?;
            Placed {
                created: Some(Created::Workspace(created.workspace.workspace_id.clone())),
                pane_id: created.root_pane.pane_id,
                tab_id: created.tab.tab_id,
                workspace_id: created.workspace.workspace_id,
            }
        }
    };
    if let Err(err) = provenance::tag(
        client,
        &placed.pane_id,
        provenance::parent_value(resolution),
    )
    .await
    {
        placed.cleanup(client).await;
        return Err(err);
    }
    Ok(placed)
}

/// A pane ID such as `w1:p2`, as opposed to a tab ID (`w1:t1`), a workspace ID or an agent name.
fn is_pane_id(value: &str) -> bool {
    value.split_once(':').is_some_and(|(workspace, pane)| {
        !workspace.is_empty()
            && pane
                .strip_prefix('p')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

async fn resolve_of(client: &Client, of: &str) -> Result<String, HerdrError> {
    if is_pane_id(of) {
        return Ok(of.to_string());
    }
    match client.agent_get(of).await {
        Ok(agent) => Ok(agent.pane_id),
        Err(err) if err.is_target_gone() => Err(HerdrError::tool(
            "target_not_found",
            format!("where.of takes a pane ID or an agent name; {of:?} matches neither"),
        )),
        Err(err) => Err(err),
    }
}

async fn existing(
    client: &Client,
    resolution: &Resolution,
    target: &str,
) -> Result<Placed, HerdrError> {
    let mut scope = Scope::with_caller(client, resolution.clone());
    let request = TargetRequest::new(
        target,
        &[TargetKind::Pane],
        "where.pane takes a pane ID or an agent name",
    )
    .refusal_code("invalid_params");
    let Scoped {
        snapshot,
        target: resolved,
        ..
    } = scope.target(request).await?;
    let resolved = resolved.expect("target is required");
    let info = client.pane_process_info(&resolved.id).await?;
    if !info.shell_in_front_strict() {
        return Err(HerdrError::tool(
            "pane_busy",
            format!(
                "{} is running something other than its shell; nothing was typed",
                resolved.id
            ),
        ));
    }
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == resolved.id)
        .ok_or_else(|| HerdrError::tool("target_not_found", format!("{} vanished", resolved.id)))?;
    Ok(Placed {
        pane_id: pane.pane_id.clone(),
        tab_id: pane.tab_id.clone(),
        workspace_id: pane.workspace_id.clone(),
        created: None,
    })
}
