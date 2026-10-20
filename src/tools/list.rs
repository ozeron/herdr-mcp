use std::collections::HashMap;
use std::future::Future;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::task::JoinSet;

use crate::caller::resolve_caller;
use crate::herdr::{
    AgentInfo, AgentStatus, Client, HerdrError, PaneInfo, PaneProcessInfo, WorkspaceInfo,
    WorktreeList, WorktreeListParams,
};
use crate::output::truncate_head;
use crate::server::Inner;

const PARENT_TOKEN: &str = "herdr_mcp_parent";
const MAX_CMDLINE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Scope {
    All,
    Workspace,
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Kind {
    Agent,
    Process,
    Shell,
    #[serde(skip_deserializing)]
    #[schemars(skip)]
    Unknown,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListParams {
    /// `all` (default), or `workspace` / `tab` relative to the calling pane.
    pub scope: Option<Scope>,
    /// Only panes of this kind: `agent`, `process` (a non-shell foreground program) or `shell`.
    pub kind: Option<Kind>,
    /// Only panes whose status is one of these.
    pub status: Option<Vec<AgentStatus>>,
    /// Case-insensitive match on workspace, tab or pane label, or agent name.
    pub label_contains: Option<String>,
}

pub async fn run(inner: &Inner, params: ListParams) -> Result<Value, HerdrError> {
    let client = inner.client()?;
    let (snapshot, agents, resolution) = tokio::try_join!(
        client.session_snapshot(),
        client.agent_list(),
        resolve_caller(&client, &inner.caller_env, inner.ancestry.as_ref()),
    )?;

    let agent_by_pane: HashMap<&str, &AgentInfo> = agents
        .iter()
        .map(|agent| (agent.pane_id.as_str(), agent))
        .collect();

    let probe_ids = snapshot
        .panes
        .iter()
        .filter(|pane| !agent_by_pane.contains_key(pane.pane_id.as_str()))
        .map(|pane| pane.pane_id.clone())
        .collect();
    let explain_ids = agents
        .iter()
        .filter(|agent| agent.agent_status == AgentStatus::Blocked)
        .map(|agent| agent.pane_id.clone())
        .collect();
    let worktree_ids = snapshot
        .workspaces
        .iter()
        .filter(|workspace| workspace.worktree.is_some())
        .map(|workspace| workspace.workspace_id.clone())
        .collect();
    let (processes, explains, worktrees) = tokio::join!(
        fan_out(inner, probe_ids, |client, id| async move {
            let info = client.pane_process_info(Some(&id)).await;
            (id, info)
        }),
        fan_out(inner, explain_ids, |client, id| async move {
            let explain = client.agent_explain(&id).await;
            (id, explain)
        }),
        fan_out(inner, worktree_ids, |client, id| async move {
            let params = WorktreeListParams {
                workspace_id: Some(id.clone()),
                ..WorktreeListParams::default()
            };
            let list = client.worktree_list(&params).await;
            (id, list)
        }),
    );

    let caller_pane = resolution.caller.as_ref().map(|c| c.pane_id.as_str());
    let caller_location =
        caller_pane.map(
            |id| match snapshot.panes.iter().find(|pane| pane.pane_id == id) {
                Some(pane) => (pane.workspace_id.clone(), pane.tab_id.clone()),
                None => {
                    let caller = resolution.caller.as_ref().unwrap();
                    (
                        caller.workspace_id.clone().unwrap_or_default(),
                        caller.tab_id.clone().unwrap_or_default(),
                    )
                }
            },
        );
    let scope = params.scope.unwrap_or(Scope::All);
    if scope != Scope::All && caller_location.is_none() {
        return Err(HerdrError::tool(
            "no_caller",
            "scope needs a caller pane, and none is known; use scope all",
        ));
    }
    let needle = params.label_contains.as_deref().map(str::to_lowercase);

    let mut workspaces = Vec::new();
    for workspace in &snapshot.workspaces {
        let branch = workspace_branch(workspace, worktrees.get(&workspace.workspace_id));
        let mut tabs = Vec::new();
        for tab in snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace.workspace_id)
        {
            let in_scope = match (&caller_location, scope) {
                (Some((ws, _)), Scope::Workspace) => *ws == tab.workspace_id,
                (Some((_, t)), Scope::Tab) => *t == tab.tab_id,
                _ => true,
            };
            if !in_scope {
                continue;
            }
            let panes: Vec<Value> = snapshot
                .panes
                .iter()
                .filter(|pane| pane.tab_id == tab.tab_id)
                .filter_map(|pane| {
                    let agent = agent_by_pane.get(pane.pane_id.as_str()).copied();
                    let kind = match agent {
                        Some(_) => Kind::Agent,
                        None => processes.get(&pane.pane_id).map_or(Kind::Unknown, classify),
                    };
                    let status = agent.map_or(pane.agent_status, |a| a.agent_status);
                    let wanted = params.kind.is_none_or(|k| k == kind)
                        && params
                            .status
                            .as_ref()
                            .is_none_or(|statuses| statuses.contains(&status))
                        && needle.as_ref().is_none_or(|needle| {
                            [
                                Some(workspace.label.as_str()),
                                Some(tab.label.as_str()),
                                pane.label.as_deref(),
                                agent.and_then(|a| a.name.as_deref()),
                            ]
                            .into_iter()
                            .flatten()
                            .any(|text| text.to_lowercase().contains(needle))
                        });
                    wanted.then(|| {
                        pane_json(
                            pane,
                            agent,
                            kind,
                            status,
                            Extras {
                                process: processes.get(&pane.pane_id),
                                explain: explains.get(&pane.pane_id),
                                branch: branch.as_deref(),
                            },
                            caller_pane == Some(pane.pane_id.as_str()),
                        )
                    })
                })
                .collect();
            if panes.is_empty() {
                continue;
            }
            tabs.push(json!({
                "id": tab.tab_id,
                "label": tab.label,
                "number": tab.number,
                "focused": tab.focused,
                "status": tab.agent_status,
                "panes": panes,
            }));
        }
        if tabs.is_empty() {
            continue;
        }
        let mut entry = json!({
            "id": workspace.workspace_id,
            "label": workspace.label,
            "number": workspace.number,
            "focused": workspace.focused,
            "status": workspace.agent_status,
            "tabs": tabs,
        });
        if let Some(worktree) = &workspace.worktree {
            entry["worktree"] = json!({
                "repo": worktree.repo_name,
                "path": worktree.checkout_path,
                "linked": worktree.is_linked_worktree,
                "branch": branch,
            });
        }
        workspaces.push(entry);
    }

    Ok(json!({
        "caller": resolution.caller.as_ref().map(|caller| json!({
            "pane_id": caller.pane_id,
            "verified": resolution.caller_verified,
        })),
        "workspaces": workspaces,
    }))
}

/// Runs one Herdr call per id on its own connection; a failed call leaves its id out.
async fn fan_out<T, F, Fut>(inner: &Inner, ids: Vec<String>, call: F) -> HashMap<String, T>
where
    T: Send + 'static,
    F: Fn(Client, String) -> Fut,
    Fut: Future<Output = (String, Result<T, HerdrError>)> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    for id in ids {
        if let Ok(client) = inner.client() {
            tasks.spawn(call(client, id));
        }
    }
    let mut results = HashMap::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((id, Ok(value))) = joined {
            results.insert(id, value);
        }
    }
    results
}

fn workspace_branch(workspace: &WorkspaceInfo, list: Option<&WorktreeList>) -> Option<String> {
    let path = &workspace.worktree.as_ref()?.checkout_path;
    list?
        .worktrees
        .iter()
        .find(|worktree| worktree.path == *path)?
        .branch
        .clone()
}

fn classify(info: &PaneProcessInfo) -> Kind {
    match (info.shell_pid, info.foreground_process_group_id) {
        (Some(shell), Some(foreground)) if shell == foreground => Kind::Shell,
        (Some(_), Some(_)) => Kind::Process,
        _ => Kind::Unknown,
    }
}

#[derive(Clone, Copy)]
struct Extras<'a> {
    process: Option<&'a PaneProcessInfo>,
    explain: Option<&'a Value>,
    branch: Option<&'a str>,
}

fn pane_json(
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    kind: Kind,
    status: AgentStatus,
    extras: Extras,
    is_caller: bool,
) -> Value {
    let Extras {
        process,
        explain,
        branch,
    } = extras;
    let mut out = Map::new();
    out.insert("id".into(), json!(pane.pane_id));
    out.insert("kind".into(), json!(kind));
    out.insert("status".into(), json!(status));
    out.insert("focused".into(), json!(pane.focused));
    if let Some(cwd) = &pane.cwd {
        out.insert("cwd".into(), json!(cwd));
    }
    if let Some(label) = &pane.label {
        out.insert("label".into(), json!(label));
    }
    if let Some(branch) = branch {
        out.insert("branch".into(), json!(branch));
    }
    if is_caller {
        out.insert("caller".into(), json!(true));
    }
    if let Some(agent) = agent {
        if let Some(kind) = &agent.agent {
            out.insert("agent".into(), json!(kind));
        }
        if let Some(name) = &agent.name {
            out.insert("name".into(), json!(name));
        }
        if let Some(parent) = agent.tokens.get(PARENT_TOKEN) {
            out.insert("launched_by".into(), json!(parent));
        }
    }
    if let Some(reason) = explain.and_then(blocked_reason) {
        out.insert("blocked_reason".into(), json!(reason));
    }
    if kind == Kind::Process
        && let Some(process) = process
        && let Some(front) = foreground(process)
    {
        out.insert("foreground".into(), front);
    }
    Value::Object(out)
}

fn foreground(info: &PaneProcessInfo) -> Option<Value> {
    let front = info
        .foreground_processes
        .iter()
        .find(|process| Some(process.pid) == info.foreground_process_group_id)
        .or_else(|| info.foreground_processes.first())?;
    let mut out = json!({"name": front.name});
    if let Some(cmdline) = &front.cmdline {
        out["cmdline"] = json!(truncate_head(cmdline, MAX_CMDLINE_CHARS));
    }
    Some(out)
}

/// The id of the matched detection rule; the full explanation carries screen previews.
fn blocked_reason(explain: &Value) -> Option<String> {
    let matched = &explain["matched_rule"];
    matched["id"]
        .as_str()
        .or_else(|| matched.as_str())
        .or_else(|| {
            explain["evaluated_rules"]
                .as_array()?
                .iter()
                .find(|rule| rule["matched"].as_bool() == Some(true))?["id"]
                .as_str()
        })
        .or_else(|| explain["fallback_reason"].as_str())
        .map(str::to_string)
}
