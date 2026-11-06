use std::collections::HashSet;

use rmcp::RoleServer;
use rmcp::schemars::JsonSchema;
use rmcp::service::RequestContext;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, Instant, sleep, timeout_at};

use crate::guard::TargetKind;
use crate::herdr::{AgentInfo, Client, HerdrError, SubscriptionEvent};
use crate::scope::{Scope, TargetRequest};
use crate::server::Inner;
use crate::tools::{DEFAULT_TIMEOUT, clamp_timeout};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct Target {
    /// Agent name or pane ID.
    pub target: String,
    /// Last state_change_seq seen for this agent, from herdr_list or a prior wait.
    pub after: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct Params {
    /// One to sixteen agents. Each must carry its own observed state_change_seq.
    pub targets: Vec<Target>,
    /// How long to wait, in milliseconds; capped at the client's wait cap.
    pub timeout_ms: Option<u64>,
    /// Allow your own pane among the targets only when the user asked.
    pub allow_self: Option<bool>,
}

struct Watch {
    pane: String,
    after: u64,
}

pub async fn run(
    inner: &Inner,
    ctx: &RequestContext<RoleServer>,
    params: Params,
) -> Result<Value, HerdrError> {
    if params.targets.is_empty() || params.targets.len() > 16 {
        return Err(HerdrError::invalid_params(
            "targets must contain 1 to 16 agents",
        ));
    }
    let client_name = ctx.client_info().map(|info| info.name);
    let (cap, note) = clamp_timeout(
        params.timeout_ms,
        inner.profile.wait_cap(client_name.as_deref()),
        DEFAULT_TIMEOUT,
    );
    let mut scope = Scope::open(inner)?;
    let mut watches = Vec::with_capacity(params.targets.len());
    let mut unique = HashSet::new();
    for target in params.targets {
        let request = TargetRequest::new(
            &target.target,
            &[TargetKind::Pane],
            "herdr_wait_change targets agents in panes",
        )
        .allow_self(params.allow_self.unwrap_or(false))
        .self_guard_first();
        let scoped = scope.target(request).await?;
        if scoped.agent().is_none() {
            return Err(HerdrError::tool(
                "agent_not_found",
                "herdr_wait_change requires a live agent at every target",
            ));
        }
        let pane = scoped.target.id.clone();
        if !unique.insert(pane.clone()) {
            return Err(HerdrError::invalid_params(
                "targets must resolve to distinct panes",
            ));
        }
        watches.push(Watch {
            pane,
            after: target.after,
        });
    }
    let client = scope.client();
    let deadline = Instant::now() + cap;
    let result = tokio::select! {
        result = observe(client, &watches, deadline) => result?,
        () = ctx.ct.cancelled() => return Err(HerdrError::tool("cancelled", "the client cancelled the wait")),
    };
    let mut out = result;
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

async fn scan(
    client: &Client,
    watches: &[Watch],
    deadline: Instant,
) -> Result<Vec<AgentInfo>, HerdrError> {
    let mut agents = Vec::with_capacity(watches.len());
    for watch in watches {
        let agent = timeout_at(deadline, client.agent_get(&watch.pane))
            .await
            .map_err(|_| {
                HerdrError::tool("herdr_timeout", "agent state lookup exceeded wait deadline")
            })??;
        agents.push(agent);
    }
    Ok(agents)
}

fn response(watches: &[Watch], agents: &[AgentInfo], timed_out: bool) -> Value {
    let mut current = Vec::with_capacity(watches.len());
    let mut changed = Vec::new();
    for (watch, agent) in watches.iter().zip(agents) {
        let entry = json!({
            "pane": watch.pane, "status": agent.agent_status,
            "state_change_seq": agent.state_change_seq,
        });
        if agent.state_change_seq > watch.after {
            changed.push(entry.clone());
        }
        current.push(entry);
    }
    json!({"matched": !changed.is_empty(), "timed_out": timed_out, "changed": changed, "current": current})
}

async fn observe(
    client: &Client,
    watches: &[Watch],
    deadline: Instant,
) -> Result<Value, HerdrError> {
    let subscriptions: Vec<_> = watches
        .iter()
        .map(|watch| {
            json!({
                "type": "pane.agent_status_changed", "pane_id": watch.pane,
            })
        })
        .collect();
    loop {
        // Subscribe before reading state, so a change during the baseline scan remains queued.
        let mut sub = timeout_at(deadline, client.subscribe(subscriptions.clone()))
            .await
            .map_err(|_| {
                HerdrError::tool("herdr_timeout", "subscription exceeded wait deadline")
            })??;
        loop {
            let agents = scan(client, watches, deadline).await?;
            if watches
                .iter()
                .zip(&agents)
                .any(|(watch, agent)| agent.state_change_seq < watch.after)
            {
                return Err(HerdrError::tool(
                    "stale_sequence",
                    "an agent's state_change_seq is lower than after; refresh targets with herdr_list",
                ));
            }
            let result = response(watches, &agents, false);
            if result["matched"] == true {
                return Ok(result);
            }
            let event = match timeout_at(deadline, sub.next()).await {
                Ok(event) => event,
                Err(_) => return Ok(response(watches, &agents, true)),
            };
            match event {
                Some(SubscriptionEvent::Event(_)) => continue,
                Some(SubscriptionEvent::EventsLost) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(response(watches, &agents, true));
                    }
                    sleep(remaining.min(Duration::from_millis(100))).await;
                    break;
                }
                Some(SubscriptionEvent::Error(err)) => return Err(err),
                Some(SubscriptionEvent::Closed) | None => {
                    return Err(HerdrError::tool(
                        "subscription_closed",
                        "Herdr closed the change stream; retry with the same after values",
                    ));
                }
            }
        }
    }
}
