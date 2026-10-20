use std::process::Command;

use crate::herdr::{Client, HerdrError};

const MAX_ANCESTOR_DEPTH: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerEnv {
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
    pub herdr_env_present: bool,
}

impl CallerEnv {
    pub fn from_process() -> Self {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|value| !value.is_empty())
        }
        let pane_id = var("HERDR_PANE_ID");
        let tab_id = var("HERDR_TAB_ID");
        let workspace_id = var("HERDR_WORKSPACE_ID");
        let herdr_env_present = var("HERDR_ENV").is_some()
            || pane_id.is_some()
            || tab_id.is_some()
            || workspace_id.is_some()
            || var("HERDR_SOCKET_PATH").is_some();
        Self {
            pane_id,
            tab_id,
            workspace_id,
            herdr_env_present,
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
        match client.pane_process_info(Some(pane_id)).await {
            Ok(info) if is_ancestor(info.shell_pid) => {
                return Ok(verified(Caller {
                    pane_id: pane_id.clone(),
                    tab_id: env.tab_id.clone(),
                    workspace_id: env.workspace_id.clone(),
                }));
            }
            Ok(_) => {}
            Err(err @ HerdrError::NotRunning { .. }) => return Err(err),
            Err(_) => {}
        }
    }

    let snapshot = client.session_snapshot().await?;
    for pane in &snapshot.panes {
        let Ok(info) = client.pane_process_info(Some(&pane.pane_id)).await else {
            continue;
        };
        if is_ancestor(info.shell_pid) {
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

fn verified(caller: Caller) -> Resolution {
    Resolution {
        guard_target: Some(caller.pane_id.clone()),
        caller: Some(caller),
        caller_verified: true,
    }
}
