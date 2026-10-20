use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};

use crate::herdr::{
    AgentInfo, AgentStatus, ApiErrorCode, Client, HerdrError, Subscription, SubscriptionEvent,
};
use crate::matcher::{MatchOutcome, Matcher, POLL_INTERVAL, read_text, tail, wait_for_match};
use crate::turn::{Turn, TurnFrom, Verdict, evaluate};

pub const AGENT_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(60);
pub const OBSERVE_WINDOW: Duration = Duration::from_secs(2);
pub const RECHECK_DELAY: Duration = Duration::from_millis(300);

const UNKNOWN_NOTE: &str =
    "status is unknown: read the pane before concluding anything about the turn";

#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Fallback `agent.get` poll for agent waits, backing up the event wake-ups.
    pub poll_interval: Duration,
    /// How often a client that sent a progress token hears from the wait.
    pub progress_interval: Duration,
    /// How long `process_exit` without a pgid looks for a non-shell foreground group.
    pub observe_window: Duration,
    /// The second look that confirms the shell is really in front.
    pub recheck_delay: Duration,
    /// Poll for waits that read the pane: `output` and `process_exit`.
    pub pane_interval: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            poll_interval: AGENT_POLL_INTERVAL,
            progress_interval: PROGRESS_INTERVAL,
            observe_window: OBSERVE_WINDOW,
            recheck_delay: RECHECK_DELAY,
            pane_interval: POLL_INTERVAL,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Condition {
    Turn(Turn),
    Until(Vec<AgentStatus>),
    Output {
        needle: String,
        after_command: Option<String>,
    },
    ProcessExit {
        pgid: Option<u32>,
    },
}

#[derive(Debug, Clone)]
pub struct Request {
    /// A pane ID. Agent names are resolved by the caller.
    pub pane: String,
    pub condition: Condition,
    /// Already clamped to the profile's wait cap.
    pub cap: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub elapsed: Duration,
    pub cap: Duration,
}

/// Runs one wait. Dropping the future drops its connections, which is how cancel works.
pub async fn wait(
    client: &Client,
    tuning: &Tuning,
    request: Request,
    progress: Option<UnboundedSender<Progress>>,
) -> Result<Value, HerdrError> {
    let started = Instant::now();
    let cap = request.cap;
    let work = dispatch(client, tuning, request, started + cap);
    tokio::pin!(work);
    let Some(progress) = progress else {
        return work.await;
    };
    let mut ticker = interval_at(started + tuning.progress_interval, tuning.progress_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            out = &mut work => return out,
            _ = ticker.tick() => {
                let _ = progress.send(Progress { elapsed: started.elapsed(), cap });
            }
        }
    }
}

/// Entry point for a caller that just prompted an agent and holds its turn token.
pub async fn wait_turn(
    client: &Client,
    tuning: &Tuning,
    turn: Turn,
    cap: Duration,
) -> Result<Value, HerdrError> {
    let request = Request {
        pane: turn.pane.clone(),
        condition: Condition::Turn(turn),
        cap,
    };
    wait(client, tuning, request, None).await
}

async fn dispatch(
    client: &Client,
    tuning: &Tuning,
    request: Request,
    deadline: Instant,
) -> Result<Value, HerdrError> {
    match request.condition {
        Condition::Turn(turn) => wait_for_turn(client, tuning, turn, deadline).await,
        Condition::Until(statuses) => {
            wait_for_status(client, tuning, &request.pane, &statuses, deadline).await
        }
        Condition::Output {
            needle,
            after_command,
        } => {
            wait_for_output(
                client,
                tuning,
                &request.pane,
                &needle,
                after_command.as_deref(),
                deadline,
            )
            .await
        }
        Condition::ProcessExit { pgid } => {
            wait_for_process(client, tuning, &request.pane, pgid, deadline).await
        }
    }
}

fn not_running() -> HerdrError {
    HerdrError::tool("agent_not_running", "the pane or its agent is gone")
}

fn stale() -> HerdrError {
    HerdrError::tool(
        "turn_stale",
        "Herdr restarted or the pane was replaced since this turn began; read the pane to see where the agent is",
    )
}

/// A closed pane is `agent_not_running` however the wait found out.
fn closed(err: HerdrError) -> HerdrError {
    match err {
        HerdrError::Api {
            code: ApiErrorCode::PaneNotFound | ApiErrorCode::AgentNotRunning,
            ..
        } => not_running(),
        other => other,
    }
}

/// Event and poll wake-ups for one pane. Events say "look again"; they never decide anything.
struct Wakeups<'a> {
    client: &'a Client,
    subscriptions: Vec<Value>,
    sub: Option<Subscription>,
    poll: Duration,
}

impl<'a> Wakeups<'a> {
    /// Subscribes first, so nothing between the subscription and the baseline read is missed.
    async fn open(client: &'a Client, pane: &str, poll: Duration) -> Result<Self, HerdrError> {
        let subscriptions = vec![
            json!({"type": "pane.agent_status_changed", "pane_id": pane}),
            json!({"type": "pane.closed", "pane_id": pane}),
        ];
        let sub = match client.subscribe(subscriptions.clone()).await {
            Ok(sub) => Some(sub),
            Err(err @ HerdrError::NotRunning { .. }) => return Err(err),
            Err(_) => None,
        };
        Ok(Self {
            client,
            subscriptions,
            sub,
            poll,
        })
    }

    async fn resubscribe(&mut self) {
        self.sub = self.client.subscribe(self.subscriptions.clone()).await.ok();
    }

    /// Returns when something may have changed, or when `deadline` passes.
    async fn next(&mut self, deadline: Instant) {
        let limit = self
            .poll
            .min(deadline.saturating_duration_since(Instant::now()));
        let Some(sub) = self.sub.as_mut() else {
            sleep(limit).await;
            self.resubscribe().await;
            return;
        };
        let mut event = tokio::select! {
            event = sub.next() => event,
            () = sleep(limit) => return,
        };
        // Queued events all mean "look again", so they fold into this wake-up instead of costing
        // a lookup each. A lost or closed stream ends the drain and is handled as before.
        while let Some(SubscriptionEvent::Event(_)) = event {
            match sub.try_next() {
                Some(queued) => event = Some(queued),
                None => return,
            }
        }
        match event {
            Some(SubscriptionEvent::EventsLost) => self.resubscribe().await,
            _ => self.sub = None,
        }
    }
}

enum Lost {
    Stale,
    NotRunning,
}

impl Lost {
    fn into_error(self) -> HerdrError {
        match self {
            Lost::Stale => stale(),
            Lost::NotRunning => not_running(),
        }
    }
}

/// Reads the agent in `pane`. A pane that moved is found again by `terminal_id`.
async fn lookup(
    client: &Client,
    pane: &mut String,
    terminal_id: &mut Option<String>,
) -> Result<Result<AgentInfo, Lost>, HerdrError> {
    let mismatch = match client.agent_get(pane).await {
        Ok(agent) => match terminal_id {
            None => {
                *terminal_id = Some(agent.terminal_id.clone());
                return Ok(Ok(agent));
            }
            Some(known) if *known == agent.terminal_id => return Ok(Ok(agent)),
            Some(_) => true,
        },
        Err(err) if err.is_target_gone() => false,
        Err(err) => return Err(err),
    };
    let Some(known) = terminal_id.as_deref() else {
        return Ok(Err(Lost::NotRunning));
    };
    if let Ok(agents) = client.agent_list().await
        && let Some(agent) = agents.into_iter().find(|agent| agent.terminal_id == known)
    {
        pane.clone_from(&agent.pane_id);
        return Ok(Ok(agent));
    }
    if mismatch {
        return Ok(Err(Lost::Stale));
    }
    match client.session_snapshot().await {
        Ok(snapshot) => Ok(Err(
            match snapshot
                .panes
                .iter()
                .find(|candidate| candidate.pane_id == *pane)
            {
                Some(found) if found.terminal_id != known => Lost::Stale,
                _ => Lost::NotRunning,
            },
        )),
        Err(err @ HerdrError::NotRunning { .. }) => Err(err),
        Err(_) => Ok(Err(Lost::NotRunning)),
    }
}

async fn shell_pid(client: &Client, pane: &str) -> Option<u32> {
    client.pane_process_info(pane).await.ok()?.shell_pid
}

async fn excerpt(client: &Client, pane: &str) -> String {
    read_text(client, pane)
        .await
        .map(|text| tail(&text))
        .unwrap_or_default()
}

async fn agent_result(
    client: &Client,
    agent: &AgentInfo,
    matched: bool,
    superseded: Option<bool>,
    turn: Option<&Turn>,
) -> Value {
    let mut out = json!({
        "matched": matched,
        "timed_out": !matched,
        "pane": agent.pane_id,
        "status": agent.agent_status,
        "state_change_seq": agent.state_change_seq,
        "completion_seq": agent.completion_seq,
        "excerpt": excerpt(client, &agent.pane_id).await,
    });
    if let Some(superseded) = superseded {
        out["superseded"] = json!(superseded);
    }
    if let Some(turn) = turn {
        out["turn"] = json!(turn);
    }
    if matched && agent.agent_status == AgentStatus::Unknown {
        out["note"] = json!(UNKNOWN_NOTE);
    }
    out
}

/// Each snapshot is judged by `evaluate`; a blocked-then-approved agent re-bases the token.
async fn wait_for_turn(
    client: &Client,
    tuning: &Tuning,
    mut turn: Turn,
    deadline: Instant,
) -> Result<Value, HerdrError> {
    let mut wakeups = Wakeups::open(client, &turn.pane, tuning.poll_interval).await?;
    let mut pane = turn.pane.clone();
    let mut terminal_id = Some(turn.terminal_id.clone());
    loop {
        // The shell probe runs beside the lookup, on the pane as last known. If the lookup finds
        // the agent in another pane, the probe is repeated there.
        let probed = pane.clone();
        let (found, early_shell) =
            tokio::join!(lookup(client, &mut pane, &mut terminal_id), async {
                match turn.shell_pid {
                    Some(_) => Some(shell_pid(client, &probed).await),
                    None => None,
                }
            });
        let agent = found?.map_err(Lost::into_error)?;
        turn.pane.clone_from(&agent.pane_id);
        let shell = match (turn.shell_pid, early_shell) {
            (Some(_), Some(shell)) if agent.pane_id == probed => shell,
            (Some(_), _) => shell_pid(client, &agent.pane_id).await,
            (None, _) => None,
        };
        match evaluate(&turn, &agent, shell) {
            Verdict::Stale => return Err(stale()),
            Verdict::Matched { superseded } => {
                let next = (agent.agent_status == AgentStatus::Blocked).then(|| Turn {
                    after: agent.state_change_seq,
                    from: TurnFrom::Blocked,
                    ..turn.clone()
                });
                return Ok(
                    agent_result(client, &agent, true, Some(superseded), next.as_ref()).await,
                );
            }
            Verdict::Rebase(rebased) => turn = rebased,
            Verdict::Pending => {}
        }
        if Instant::now() >= deadline {
            return Ok(agent_result(client, &agent, false, None, Some(&turn)).await);
        }
        wakeups.next(deadline).await;
    }
}

async fn wait_for_status(
    client: &Client,
    tuning: &Tuning,
    pane: &str,
    statuses: &[AgentStatus],
    deadline: Instant,
) -> Result<Value, HerdrError> {
    let mut wakeups = Wakeups::open(client, pane, tuning.poll_interval).await?;
    let mut pane = pane.to_string();
    let mut terminal_id = None;
    loop {
        let agent = lookup(client, &mut pane, &mut terminal_id)
            .await?
            .map_err(|_| not_running())?;
        if statuses.contains(&agent.agent_status) {
            return Ok(agent_result(client, &agent, true, None, None).await);
        }
        if Instant::now() >= deadline {
            return Ok(agent_result(client, &agent, false, None, None).await);
        }
        wakeups.next(deadline).await;
    }
}

async fn wait_for_output(
    client: &Client,
    tuning: &Tuning,
    pane: &str,
    needle: &str,
    after_command: Option<&str>,
    deadline: Instant,
) -> Result<Value, HerdrError> {
    let first = read_text(client, pane).await.map_err(closed)?;
    let mut note = None;
    let matcher = match after_command {
        Some(command) => {
            let anchored = Matcher::after_command(needle, command);
            if anchored.anchor_missing(&first) {
                note = Some(
                    "the echoed command is no longer in the pane's recent output, so only output added from now on can match",
                );
                Matcher::new(&first, needle, "")
            } else {
                anchored
            }
        }
        None => Matcher::new(&first, needle, ""),
    };
    let timeout = deadline.saturating_duration_since(Instant::now());
    let outcome = wait_for_match(&matcher, timeout, tuning.pane_interval, || async {
        read_text(client, pane).await.map_err(closed)
    })
    .await?;
    let mut out = match outcome {
        MatchOutcome::Matched { excerpt } => {
            json!({"matched": true, "timed_out": false, "pane": pane, "excerpt": excerpt})
        }
        MatchOutcome::TimedOut { tail } => {
            json!({"matched": false, "timed_out": true, "pane": pane, "excerpt": tail})
        }
    };
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

/// Matches when the tracked process group has left the foreground and a recheck still
/// finds the shell in front, so `a && b` does not match between jobs.
async fn wait_for_process(
    client: &Client,
    tuning: &Tuning,
    pane: &str,
    pgid: Option<u32>,
    deadline: Instant,
) -> Result<Value, HerdrError> {
    let observe_until = Instant::now() + tuning.observe_window;
    let mut tracked = pgid;
    let done = |tracked: Option<u32>, matched: bool, timed_out: bool| json!({"matched": matched, "timed_out": timed_out, "pane": pane, "pgid": tracked});
    loop {
        let info = client.pane_process_info(pane).await.map_err(closed)?;
        match tracked {
            None if !info.shell_in_front_or_unknown() => tracked = info.foreground_process_group_id,
            None => {
                if Instant::now() >= observe_until {
                    let mut out = done(None, false, false);
                    out["result"] = json!("no_active_work");
                    return Ok(out);
                }
            }
            Some(group) => {
                if info.foreground_process_group_id != Some(group)
                    && info.shell_in_front_or_unknown()
                {
                    sleep(tuning.recheck_delay).await;
                    let again = client.pane_process_info(pane).await.map_err(closed)?;
                    if again.shell_in_front_or_unknown() {
                        return Ok(done(tracked, true, false));
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Ok(done(tracked, false, true));
        }
        sleep(
            tuning
                .pane_interval
                .min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}
