use std::collections::HashMap;
use std::path::{Component, PathBuf};

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
use crate::scope::{Scope, TargetRequest};

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
    /// A new tab. This is the default. It goes into `workspace_id` when given, else the calling pane's workspace; a caller with no verified pane (a remote client) must give `workspace_id`.
    pub tab: Option<bool>,
    /// Workspace ID, or label matched case-insensitively (an ID wins), from herdr_list, for a new tab or worktree. Wins over the calling pane's workspace. Required for remote callers. A cwd outside this workspace's worktree is allowed and returns a warning.
    pub workspace_id: Option<String>,
    /// A new workspace.
    pub workspace: Option<bool>,
    /// A new Herdr worktree and workspace; its root pane is used.
    pub worktree: Option<WorktreeWhere>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The caller is a remote client: a new tab needs an explicit or caller workspace.
    pub remote: bool,
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
    /// The new pane's working directory, as Herdr reports it, else the one asked for.
    pub cwd: Option<String>,
    /// Set when the cwd lies outside the destination workspace's worktree.
    pub warning: Option<String>,
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
    if where_.workspace_id.is_some() && !matches!(plan, Plan::Tab | Plan::Worktree(_)) {
        return Err(HerdrError::invalid_params(
            "workspace_id places a new tab or worktree; it does not apply to pane, split or a new workspace",
        ));
    }
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
    let requested = where_.and_then(|w| w.workspace_id.as_deref());
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
                cwd: pane.cwd.or_else(|| options.cwd.clone()),
                pane_id: pane.pane_id,
                tab_id: pane.tab_id,
                workspace_id: pane.workspace_id,
                warning: None,
            }
        }
        Plan::Tab => {
            let dest = destination(
                client,
                requested,
                caller_workspace.as_deref(),
                options.cwd.as_deref(),
                options.remote,
            )
            .await?;
            let params = TabCreateParams {
                workspace_id: Some(dest.workspace_id.clone()),
                cwd: options.cwd.clone(),
                label: options.label.clone(),
                env: options.env.clone(),
                ..TabCreateParams::default()
            };
            let created = client.tab_create(&params).await?;
            Placed {
                created: Some(Created::Tab(created.tab.tab_id.clone())),
                cwd: created.root_pane.cwd.or_else(|| options.cwd.clone()),
                pane_id: created.root_pane.pane_id,
                tab_id: created.tab.tab_id,
                workspace_id: created.tab.workspace_id,
                warning: dest
                    .note
                    .clone()
                    .or_else(|| dest.cwd_warning(options.cwd.as_deref())),
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
                cwd: created.root_pane.cwd.or_else(|| options.cwd.clone()),
                pane_id: created.root_pane.pane_id,
                tab_id: created.tab.tab_id,
                workspace_id: created.workspace.workspace_id,
                warning: None,
            }
        }
        Plan::Worktree(worktree) => {
            let dest = destination(
                client,
                requested,
                caller_workspace.as_deref(),
                options.cwd.as_deref(),
                options.remote,
            )
            .await?;
            let dest_note = dest.note.clone();
            let worktree_warning = dest.cwd_warning(options.cwd.as_deref());
            let params = WorktreeCreateParams {
                workspace_id: Some(dest.workspace_id),
                cwd: options.cwd.clone(),
                branch: Some(worktree.branch.clone()),
                base: worktree.base.clone(),
                label: options.label.clone(),
                ..WorktreeCreateParams::default()
            };
            let created = client.worktree_create(&params).await?;
            Placed {
                created: Some(Created::Workspace(created.workspace.workspace_id.clone())),
                cwd: created.root_pane.cwd.or_else(|| options.cwd.clone()),
                pane_id: created.root_pane.pane_id,
                tab_id: created.tab.tab_id,
                workspace_id: created.workspace.workspace_id,
                warning: dest_note.or(worktree_warning),
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
    let scoped = scope.target(request).await?;
    let resolved = &scoped.target;
    let snapshot = &scoped.snapshot;
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
        cwd: pane.cwd.clone(),
        warning: None,
    })
}

/// The workspace a new tab or worktree goes into, with its worktree path for the cwd check.
#[derive(Debug, Clone)]
pub struct Destination {
    pub workspace_id: String,
    pub worktree_path: Option<String>,
    /// Set when a local caller with no verified pane fell back to the focused workspace.
    pub note: Option<String>,
}

impl Destination {
    /// A warning when `cwd` lies outside this workspace's worktree. The tab still opens.
    pub fn cwd_warning(&self, cwd: Option<&str>) -> Option<String> {
        let (cwd, root) = (cwd?, self.worktree_path.as_deref()?);
        (!real_path(cwd).starts_with(real_path(root))).then(|| {
            format!(
                "cwd {cwd} is outside workspace {}'s worktree {root}; the tab opened there anyway",
                self.workspace_id
            )
        })
    }
}

/// `path` as the file system sees it: `~` expanded, `..` folded, and symlinks such as `/tmp`
/// resolved in the longest part that exists, so the cwd check compares real locations.
fn real_path(path: &str) -> PathBuf {
    let expanded = match (path.strip_prefix('~'), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    };
    let mut folded = PathBuf::new();
    for part in expanded.components() {
        match part {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    // Resolve the longest part that exists, so /tmp/new matches /private/tmp/new.
    let mut rest = Vec::new();
    let mut base = folded.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return folded,
        }
    }
}

/// Resolves where a new tab or worktree goes: `requested` (a workspace ID or label, matched
/// case-insensitively), else the verified caller's workspace. With neither, a remote caller is
/// refused rather than sent to whatever workspace has focus, which can change between calls; a
/// local caller with no verified pane keeps the focused workspace, with a note.
pub async fn destination(
    client: &Client,
    requested: Option<&str>,
    caller_workspace: Option<&str>,
    cwd: Option<&str>,
    remote: bool,
) -> Result<Destination, HerdrError> {
    // The caller's own workspace needs no lookup unless a cwd is to be checked against it.
    if let (None, Some(own), None) = (requested, caller_workspace, cwd) {
        return Ok(Destination {
            workspace_id: own.to_string(),
            worktree_path: None,
            note: None,
        });
    }
    let snapshot = match client.session_snapshot().await {
        Ok(snapshot) => snapshot,
        // The caller's own workspace needs the snapshot only for the cwd warning: skip it.
        Err(_) if requested.is_none() && caller_workspace.is_some() => {
            return Ok(Destination {
                workspace_id: caller_workspace.unwrap_or_default().to_string(),
                worktree_path: None,
                note: None,
            });
        }
        Err(err) => return Err(err),
    };
    let workspaces = &snapshot.workspaces;
    let listing = || {
        json!(
            workspaces
                .iter()
                .map(|w| json!({"id": w.workspace_id, "label": w.label, "focused": w.focused}))
                .collect::<Vec<_>>()
        )
    };
    let found = match (requested, caller_workspace) {
        (Some(wanted), _) => {
            let by_label: Vec<_> = workspaces
                .iter()
                .filter(|w| w.label.eq_ignore_ascii_case(wanted))
                .collect();
            match workspaces.iter().find(|w| w.workspace_id == wanted) {
                Some(found) => Some(found),
                None if by_label.len() == 1 => Some(by_label[0]),
                None if by_label.len() > 1 => {
                    return Err(HerdrError::detailed(
                        "invalid_params",
                        format!(
                            "{wanted:?} matches {} workspace labels; give its ID",
                            by_label.len()
                        ),
                        json!({"workspaces": listing()}),
                    ));
                }
                None => {
                    return Err(HerdrError::detailed(
                        "target_not_found",
                        format!("no workspace has the ID or label {wanted:?}; nothing was created"),
                        json!({"workspaces": listing()}),
                    ));
                }
            }
        }
        (None, Some(own)) => workspaces.iter().find(|w| w.workspace_id == own),
        (None, None) if remote => {
            return Err(HerdrError::detailed(
                "destination_required",
                "a remote caller must give where.workspace_id (an ID or label from herdr_list); nothing was created",
                json!({"workspaces": listing()}),
            ));
        }
        (None, None) => {
            let focused = workspaces.iter().find(|w| w.focused).ok_or_else(|| {
                HerdrError::detailed(
                    "destination_required",
                    "no caller pane and no focused workspace; give where.workspace_id",
                    json!({"workspaces": listing()}),
                )
            })?;
            return Ok(Destination {
                workspace_id: focused.workspace_id.clone(),
                worktree_path: focused.worktree.as_ref().map(|w| w.checkout_path.clone()),
                note: Some(format!(
                    "no verified caller pane, so this went to the focused workspace {}; pass where.workspace_id to choose",
                    focused.workspace_id
                )),
            });
        }
    };
    Ok(match found {
        Some(workspace) => Destination {
            workspace_id: workspace.workspace_id.clone(),
            worktree_path: workspace.worktree.as_ref().map(|w| w.checkout_path.clone()),
            note: None,
        },
        // The caller's workspace is missing from the snapshot; let Herdr place it there anyway.
        None => Destination {
            workspace_id: caller_workspace.unwrap_or_default().to_string(),
            worktree_path: None,
            note: None,
        },
    })
}
