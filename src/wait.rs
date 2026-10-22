use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};

use crate::herdr::{
    AgentInfo, AgentStatus, ApiErrorCode, Client, EventEnvelope, HerdrError, Subscription,
    SubscriptionEvent,
};
use crate::matcher::{MatchOutcome, Matcher, POLL_INTERVAL, read_text, tail, wait_for_match};
use crate::turn::{Turn, TurnFrom, Verdict, evaluate};

pub const AGENT_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(60);
pub const OBSERVE_WINDOW: Duration = Duration::from_secs(2);
pub const RECHECK_DELAY: Duration = Duration::from_millis(300);

const EXCERPT_TIMEOUT: Duration = Duration::from_secs(2);
/// Herdr often takes about 100 ms to answer, so the look at the deadline gets more than that.
const LAST_LOOK: Duration = Duration::from_millis(500);
const UNKNOWN_NOTE: &str =
    "status is unknown: read the pane before concluding anything about the turn";
const LATE_NOTE: &str = "Herdr did not answer the last look in time, so the status is as last seen";

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

/// What one wait found. `to_json` is the only place the wire shape is built.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub matched: bool,
    pub timed_out: bool,
    pub pane: String,
    /// Joined with `"; "` at render, in the order they were added.
    pub notes: Vec<String>,
    pub detail: Detail,
}

#[derive(Debug, Clone)]
pub enum Detail {
    Agent(AgentDetail),
    Output {
        excerpt: String,
    },
    Process {
        pgid: Option<u32>,
        /// Set when no non-shell group ever appeared, so there was nothing to wait for.
        no_active_work: bool,
    },
}

#[derive(Debug, Clone)]
pub struct AgentDetail {
    pub status: AgentStatus,
    pub state_change_seq: u64,
    pub completion_seq: Option<u64>,
    pub excerpt: String,
    pub superseded: Option<bool>,
    /// Only when the turn settles blocked or times out: the token to wait again with.
    pub turn: Option<Turn>,
}

impl Outcome {
    pub fn add_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "matched": self.matched,
            "timed_out": self.timed_out,
            "pane": self.pane,
        });
        match &self.detail {
            Detail::Agent(agent) => {
                out["status"] = json!(agent.status);
                out["state_change_seq"] = json!(agent.state_change_seq);
                out["completion_seq"] = json!(agent.completion_seq);
                out["excerpt"] = json!(agent.excerpt);
                if let Some(superseded) = agent.superseded {
                    out["superseded"] = json!(superseded);
                }
                if let Some(turn) = &agent.turn {
                    out["turn"] = json!(turn);
                }
            }
            Detail::Output { excerpt } => out["excerpt"] = json!(excerpt),
            Detail::Process {
                pgid,
                no_active_work,
            } => {
                out["pgid"] = json!(pgid);
                if *no_active_work {
                    out["result"] = json!("no_active_work");
                }
            }
        }
        if !self.notes.is_empty() {
            out["note"] = json!(self.notes.join("; "));
        }
        out
    }
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
) -> Result<Outcome, HerdrError> {
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

async fn dispatch(
    client: &Client,
    tuning: &Tuning,
    request: Request,
    deadline: Instant,
) -> Result<Outcome, HerdrError> {
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

/// Runs one socket step within `min(the client's timeout, the time left before deadline)`, so a
/// stalled Herdr cannot push a wait more than `LAST_LOOK` past its cap at each step. A poll that wakes at the deadline still gets
/// `LAST_LOOK` for its final read, instead of failing with 0 ms.
async fn bounded<T>(
    client: &Client,
    deadline: Instant,
    step: impl Future<Output = Result<T, HerdrError>>,
) -> Result<T, HerdrError> {
    let limit = client.timeout().min(
        deadline
            .saturating_duration_since(Instant::now())
            .max(LAST_LOOK),
    );
    tokio::time::timeout(limit, step).await.unwrap_or_else(|_| {
        Err(HerdrError::tool(
            "herdr_timeout",
            format!("herdr did not answer within {} ms", limit.as_millis()),
        ))
    })
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

/// Event and poll wake-ups for one pane. Events say "look again"; a turn wait also keeps the
/// strongest status they carried, because Herdr resets `completion_seq` on each transition and a
/// lookup cannot see a `blocked` or settle that came and went between two lookups.
struct Wakeups<'a> {
    client: &'a Client,
    subscriptions: Vec<Value>,
    sub: Option<Subscription>,
    poll: Duration,
    seen: Option<AgentStatus>,
}

impl<'a> Wakeups<'a> {
    /// Subscribes first, so nothing between the subscription and the baseline read is missed.
    async fn open(
        client: &'a Client,
        pane: &str,
        poll: Duration,
        deadline: Instant,
    ) -> Result<Self, HerdrError> {
        let subscriptions = vec![
            json!({"type": "pane.agent_status_changed", "pane_id": pane}),
            json!({"type": "pane.closed", "pane_id": pane}),
        ];
        let sub = match bounded(client, deadline, client.subscribe(subscriptions.clone())).await {
            Ok(sub) => Some(sub),
            Err(err @ HerdrError::NotRunning { .. }) => return Err(err),
            Err(_) => None,
        };
        Ok(Self {
            client,
            subscriptions,
            sub,
            poll,
            seen: None,
        })
    }

    async fn resubscribe(&mut self, deadline: Instant) {
        let subscribe = self.client.subscribe(self.subscriptions.clone());
        self.sub = bounded(self.client, deadline, subscribe).await.ok();
    }

    /// Returns when something may have changed, or when `deadline` passes.
    async fn next(&mut self, deadline: Instant) {
        let limit = self
            .poll
            .min(deadline.saturating_duration_since(Instant::now()));
        let Some(sub) = self.sub.as_mut() else {
            sleep(limit).await;
            self.resubscribe(deadline).await;
            return;
        };
        let mut event = tokio::select! {
            event = sub.next() => event,
            () = sleep(limit) => return,
        };
        // Queued events all mean "look again", so they fold into this wake-up instead of costing
        // a lookup each. A lost or closed stream ends the drain and is handled as before.
        while let Some(SubscriptionEvent::Event(envelope)) = &event {
            self.seen = stronger(self.seen, event_status(envelope));
            match sub.try_next() {
                Some(queued) => event = Some(queued),
                None => return,
            }
        }
        match event {
            Some(SubscriptionEvent::EventsLost) => self.resubscribe(deadline).await,
            _ => self.sub = None,
        }
    }
}

/// The agent status an event reports, if it is a status event.
fn event_status(event: &EventEnvelope) -> Option<AgentStatus> {
    serde_json::from_value(event.data.get("agent_status")?.clone()).ok()
}

/// `blocked` outranks a settle, a settle outranks `working`, and a later settle replaces an
/// earlier one.
fn stronger(seen: Option<AgentStatus>, new: Option<AgentStatus>) -> Option<AgentStatus> {
    match (seen, new) {
        (_, None) => seen,
        (None, new) => new,
        (Some(AgentStatus::Blocked), _) => seen,
        (Some(_), Some(AgentStatus::Working)) => seen,
        (Some(_), new) => new,
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

async fn shell_pid(client: &Client, pane: &str, deadline: Instant) -> Option<u32> {
    bounded(client, deadline, client.pane_process_info(pane))
        .await
        .ok()?
        .shell_pid
}

/// The closing excerpt is a courtesy: it gets a short bound of its own, since the deadline has
/// usually passed by the time it is read.
async fn excerpt(client: &Client, pane: &str) -> String {
    let limit = client.timeout().min(EXCERPT_TIMEOUT);
    match tokio::time::timeout(limit, read_text(client, pane)).await {
        Ok(Ok(text)) => tail(&text),
        _ => String::new(),
    }
}

async fn agent_result(
    client: &Client,
    agent: &AgentInfo,
    status: AgentStatus,
    matched: bool,
    superseded: Option<bool>,
    turn: Option<&Turn>,
) -> Outcome {
    let notes = if matched && status == AgentStatus::Unknown {
        vec![UNKNOWN_NOTE.to_string()]
    } else {
        Vec::new()
    };
    Outcome {
        matched,
        timed_out: !matched,
        pane: agent.pane_id.clone(),
        notes,
        detail: Detail::Agent(AgentDetail {
            status,
            state_change_seq: agent.state_change_seq,
            completion_seq: agent.completion_seq,
            excerpt: excerpt(client, &agent.pane_id).await,
            superseded,
            turn: turn.cloned(),
        }),
    }
}

/// Each snapshot is judged by `evaluate`; a blocked-then-approved agent re-bases the token.
async fn wait_for_turn(
    client: &Client,
    tuning: &Tuning,
    mut turn: Turn,
    deadline: Instant,
) -> Result<Outcome, HerdrError> {
    let mut wakeups = Wakeups::open(client, &turn.pane, tuning.poll_interval, deadline).await?;
    let mut pane = turn.pane.clone();
    let mut terminal_id = Some(turn.terminal_id.clone());
    let mut last = None;
    loop {
        // The shell probe runs beside the lookup, on the pane as last known. If the lookup finds
        // the agent in another pane, the probe is repeated there.
        let probed = pane.clone();
        let (found, early_shell) = tokio::join!(
            bounded(
                client,
                deadline,
                lookup(client, &mut pane, &mut terminal_id)
            ),
            async {
                match turn.shell_pid {
                    Some(_) => Some(shell_pid(client, &probed, deadline).await),
                    None => None,
                }
            }
        );
        let agent = match found {
            Ok(found) => found.map_err(Lost::into_error)?,
            Err(err) => return late(client, err, deadline, last, Some(&turn)).await,
        };
        last = Some(agent.clone());
        turn.pane.clone_from(&agent.pane_id);
        let shell = match (turn.shell_pid, early_shell) {
            (Some(_), Some(shell)) if agent.pane_id == probed => shell,
            (Some(_), _) => shell_pid(client, &agent.pane_id, deadline).await,
            (None, _) => None,
        };
        let seen = wakeups.seen.take();
        match evaluate(&turn, &agent, shell) {
            Verdict::Stale => return Err(stale()),
            Verdict::Matched { superseded } => {
                // A lookup that finds the agent working again cannot tell a finished turn from one
                // that was blocked or settled in between; the events can.
                let (status, superseded) = match seen {
                    Some(seen) if superseded && seen != AgentStatus::Working => (seen, false),
                    _ => (agent.agent_status, superseded),
                };
                let next = (status == AgentStatus::Blocked).then(|| Turn {
                    after: agent.state_change_seq,
                    from: TurnFrom::Blocked,
                    ..turn.clone()
                });
                return Ok(agent_result(
                    client,
                    &agent,
                    status,
                    true,
                    Some(superseded),
                    next.as_ref(),
                )
                .await);
            }
            Verdict::Rebase(rebased) => turn = rebased,
            Verdict::Pending => {}
        }
        if Instant::now() >= deadline {
            return Ok(
                agent_result(client, &agent, agent.agent_status, false, None, Some(&turn)).await,
            );
        }
        wakeups.next(deadline).await;
    }
}

/// A lookup that runs out of time at the deadline ends the wait as a timeout on what it saw
/// last, not as an error.
async fn late(
    client: &Client,
    err: HerdrError,
    deadline: Instant,
    last: Option<AgentInfo>,
    turn: Option<&Turn>,
) -> Result<Outcome, HerdrError> {
    match last {
        Some(agent) if err.code() == "herdr_timeout" && Instant::now() >= deadline => {
            let mut out = agent_result(client, &agent, agent.agent_status, false, None, turn).await;
            out.add_note(LATE_NOTE);
            Ok(out)
        }
        _ => Err(err),
    }
}

async fn wait_for_status(
    client: &Client,
    tuning: &Tuning,
    pane: &str,
    statuses: &[AgentStatus],
    deadline: Instant,
) -> Result<Outcome, HerdrError> {
    let mut wakeups = Wakeups::open(client, pane, tuning.poll_interval, deadline).await?;
    let mut pane = pane.to_string();
    let mut terminal_id = None;
    let mut last = None;
    loop {
        let agent = match bounded(
            client,
            deadline,
            lookup(client, &mut pane, &mut terminal_id),
        )
        .await
        {
            Ok(found) => found.map_err(|_| not_running())?,
            Err(err) => return late(client, err, deadline, last, None).await,
        };
        last = Some(agent.clone());
        if statuses.contains(&agent.agent_status) {
            return Ok(agent_result(client, &agent, agent.agent_status, true, None, None).await);
        }
        if Instant::now() >= deadline {
            return Ok(agent_result(client, &agent, agent.agent_status, false, None, None).await);
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
) -> Result<Outcome, HerdrError> {
    let first = bounded(client, deadline, read_text(client, pane))
        .await
        .map_err(closed)?;
    let matcher = match after_command {
        Some(command) => Matcher::after_command(needle, command).with_fallback_baseline(&first),
        None => Matcher::new(&first, needle, ""),
    };
    let last = Mutex::new(first.clone());
    let timeout = deadline.saturating_duration_since(Instant::now());
    let outcome = wait_for_match(&matcher, timeout, tuning.pane_interval, || async {
        // The matcher's last poll can wake right at the deadline; a read that times out then
        // falls back to the last text.
        match bounded(client, deadline, read_text(client, pane)).await {
            Ok(text) => {
                *last.lock().unwrap() = text.clone();
                Ok(text)
            }
            Err(err) if err.code() == "herdr_timeout" && Instant::now() >= deadline => {
                Ok(last.lock().unwrap().clone())
            }
            Err(err) => Err(closed(err)),
        }
    })
    .await?;
    let (matched, excerpt) = match outcome {
        MatchOutcome::Matched { excerpt } => (true, excerpt),
        MatchOutcome::TimedOut { tail } => (false, tail),
    };
    let mut out = Outcome {
        matched,
        timed_out: !matched,
        pane: pane.to_string(),
        notes: Vec::new(),
        detail: Detail::Output { excerpt },
    };
    if matcher.anchor_missing(&last.lock().unwrap()) {
        out.add_note(
            "the echoed command is no longer in the pane's recent output, so only output added from the first read of this wait can match",
        );
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
) -> Result<Outcome, HerdrError> {
    let observe_until = Instant::now() + tuning.observe_window;
    let mut tracked = pgid;
    let mut unavailable;
    let done = |pgid: Option<u32>, matched: bool, timed_out: bool, no_active_work: bool| Outcome {
        matched,
        timed_out,
        pane: pane.to_string(),
        notes: Vec::new(),
        detail: Detail::Process {
            pgid,
            no_active_work,
        },
    };
    loop {
        let info = bounded(client, deadline, client.pane_process_info(pane))
            .await
            .map_err(closed)?;
        // An unreadable foreground group is not the shell in front; it proves nothing either way.
        unavailable = !info.shell_in_front_strict() && info.left_shell_group().is_none();
        match tracked {
            None => {
                if let Some(group) = info.left_shell_group() {
                    tracked = Some(group);
                } else if !unavailable && Instant::now() >= observe_until {
                    return Ok(done(None, false, false, true));
                }
            }
            Some(group) => {
                if info.foreground_process_group_id != Some(group) && info.shell_in_front_strict() {
                    sleep(tuning.recheck_delay).await;
                    let again = bounded(client, deadline, client.pane_process_info(pane))
                        .await
                        .map_err(closed)?;
                    if again.shell_in_front_strict() {
                        return Ok(done(tracked, true, false, false));
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            let mut out = done(tracked, false, true, false);
            if unavailable {
                out.add_note(
                    "process info unavailable: the foreground process group could not be read, so the process may still be running",
                );
            }
            return Ok(out);
        }
        sleep(
            tuning
                .pane_interval
                .min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}
