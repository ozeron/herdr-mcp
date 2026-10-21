use std::process::Command;
use std::sync::OnceLock;

use futures_util::StreamExt;
use futures_util::stream;

use crate::env::env_nonempty_string;
use crate::herdr::{Client, HerdrError, PaneProcessInfo, SessionSnapshot};

const MAX_ANCESTOR_DEPTH: usize = 64;
const FALLBACK_PROBES: usize = 8;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerEnv {
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
}

impl CallerEnv {
    pub fn from_process() -> Self {
        let var = env_nonempty_string;
        let pane_id = var("HERDR_PANE_ID");
        let tab_id = var("HERDR_TAB_ID");
        let workspace_id = var("HERDR_WORKSPACE_ID");
        Self {
            pane_id,
            tab_id,
            workspace_id,
        }
    }
}

pub trait Ancestry: Send + Sync {
    /// Parent PIDs of the bridge, nearest first.
    fn ancestor_pids(&self) -> Vec<u32>;
}

pub struct StaticAncestry(pub Vec<u32>);

impl Ancestry for StaticAncestry {
    fn ancestor_pids(&self) -> Vec<u32> {
        self.0.clone()
    }
}

pub struct ProcessAncestry;

impl Ancestry for ProcessAncestry {
    fn ancestor_pids(&self) -> Vec<u32> {
        // Every `ps` call is a fork; the chain is walked once per process.
        static CHAIN: OnceLock<Vec<u32>> = OnceLock::new();
        CHAIN.get_or_init(walk_ancestors).clone()
    }
}

fn walk_ancestors() -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = std::os::unix::process::parent_id();
    while current > 0 && chain.len() < MAX_ANCESTOR_DEPTH && !chain.contains(&current) {
        chain.push(current);
        match parent_of(current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    chain
}

fn parent_of(pid: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub pane_id: String,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub caller: Option<Caller>,
    pub caller_verified: bool,
    /// The pane the self-guards protect: the verified caller, or the env-named pane when unverified.
    pub guard_target: Option<String>,
}

impl Resolution {
    /// No caller: the call comes from outside the session it talks to.
    pub fn external() -> Self {
        Self {
            caller: None,
            caller_verified: false,
            guard_target: None,
        }
    }
}

/// Verifies `HERDR_PANE_ID` by checking that the pane's shell is an ancestor of the bridge,
/// then falls back to scanning every pane for an ancestor shell.
pub async fn resolve_caller(
    client: &Client,
    env: &CallerEnv,
    ancestry: &dyn Ancestry,
) -> Result<Resolution, HerdrError> {
    let ancestors = ancestry.ancestor_pids();
    let is_ancestor =
        |shell_pid: Option<u32>| shell_pid.is_some_and(|pid| pid != 0 && ancestors.contains(&pid));

    if let Some(pane_id) = &env.pane_id {
        match client.pane_process_info(pane_id).await {
            Ok(info) if is_ancestor(info.shell_pid) => {
                let mut caller = Caller {
                    pane_id: pane_id.clone(),
                    tab_id: env.tab_id.clone(),
                    workspace_id: env.workspace_id.clone(),
                };
                // A shell started outside Herdr's env may lack these; Herdr knows.
                if (caller.workspace_id.is_none() || caller.tab_id.is_none())
                    && let Ok(snapshot) = client.session_snapshot().await
                    && let Some(pane) = snapshot.panes.iter().find(|p| p.pane_id == *pane_id)
                {
                    caller
                        .workspace_id
                        .get_or_insert_with(|| pane.workspace_id.clone());
                    caller.tab_id.get_or_insert_with(|| pane.tab_id.clone());
                }
                return Ok(verified(caller));
            }
            Ok(_) => {}
            Err(err @ HerdrError::NotRunning { .. }) => return Err(err),
            Err(_) => {}
        }
    }

    let snapshot = client.session_snapshot().await?;
    // Probes run up to `FALLBACK_PROBES` at a time; `buffered` yields them in snapshot order, so
    // the first match in that order wins and later panes are not probed once it is found.
    let mut probes = stream::iter(pane_ids(&snapshot).into_iter().enumerate())
        .map(|(index, pane_id)| probe(client, index, pane_id))
        .buffered(FALLBACK_PROBES);
    while let Some((index, probe)) = probes.next().await {
        let Ok(info) = probe else {
            continue;
        };
        if is_ancestor(info.shell_pid) {
            let pane = &snapshot.panes[index];
            return Ok(verified(Caller {
                pane_id: pane.pane_id.clone(),
                tab_id: Some(pane.tab_id.clone()),
                workspace_id: Some(pane.workspace_id.clone()),
            }));
        }
    }

    Ok(Resolution {
        caller: env.pane_id.as_ref().map(|pane_id| Caller {
            pane_id: pane_id.clone(),
            tab_id: env.tab_id.clone(),
            workspace_id: env.workspace_id.clone(),
        }),
        caller_verified: false,
        guard_target: env.pane_id.clone(),
    })
}

fn pane_ids(snapshot: &SessionSnapshot) -> Vec<String> {
    snapshot
        .panes
        .iter()
        .map(|pane| pane.pane_id.clone())
        .collect()
}

/// Owned arguments and result keep the stream's closure free of borrowed-pane lifetimes.
async fn probe(
    client: &Client,
    index: usize,
    pane_id: String,
) -> (usize, Result<PaneProcessInfo, HerdrError>) {
    (index, client.pane_process_info(&pane_id).await)
}

fn verified(caller: Caller) -> Resolution {
    Resolution {
        guard_target: Some(caller.pane_id.clone()),
        caller: Some(caller),
        caller_verified: true,
    }
}
