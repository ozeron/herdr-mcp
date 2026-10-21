use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::cli::ToolMode;
use crate::delegation::{
    NO_MAIL_NOTE, ReplyWait, await_reply, mail_plan, reply_json, reply_paragraph,
    require_verified_caller, take_reply,
};
use crate::herdr::{
    AgentInfo, AgentReadParams, AgentStartParams, AgentStatus, ApiErrorCode, Client, HerdrError,
    ReadSource,
};
use crate::install::shim_path_in;
use crate::install::status::{McpStatus, global_mode, mcp_status_for};
use crate::mailbox::{Message, SenderFilter};
use crate::matcher::{read_text, tail};
use crate::place::{self, Options, Where};
use crate::provenance;
use crate::providers::{Dialog, is_ready, match_dialog, mcp_args, mode_args, runtime_args};
use crate::runtime_mode::{self, RuntimeMode};
use crate::scope::Scope;
use crate::server::Inner;
use crate::tools::launch_session::{LaunchSession, Phase};
use crate::tools::send::{gated_prompt, turn_for};
use crate::tools::{DEFAULT_TIMEOUT, clamp_timeout};
use crate::turn::TurnFrom;
use crate::wait::{self, Condition, Progress, Request};

pub const START_RETRY_INTERVAL: Duration = Duration::from_millis(100);
pub const START_RETRY_WINDOW: Duration = Duration::from_secs(2);
pub const READY_POLL: Duration = Duration::from_millis(250);
pub const QUIET_WINDOW: Duration = Duration::from_secs(3);
pub const DIALOG_SETTLE: Duration = Duration::from_secs(1);
pub const STARTUP_GRACE: Duration = Duration::from_secs(3);
pub const IDLE_POLL: Duration = Duration::from_millis(250);
pub const MAIL_POLL: Duration = Duration::from_millis(250);
pub const REPLY_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct LaunchTuning {
    /// Pause between `agent.start` attempts while a fresh pane reports busy.
    pub start_retry_interval: Duration,
    /// How long a fresh pane may keep reporting busy.
    pub start_retry_window: Duration,
    /// Poll while waiting for the agent to become ready.
    pub ready_poll: Duration,
    /// How long the agent must show no state change after first ready.
    pub quiet_window: Duration,
    /// How long a startup dialog may take to disappear after it is answered.
    pub dialog_settle: Duration,
    /// Poll while `herdr_send` waits for an idle agent.
    pub idle_poll: Duration,
    /// How long the shell may stay in front after `agent.start` before the launch counts as exited.
    pub startup_grace: Duration,
    /// Poll for the child's mail while `reply: mail` waits.
    pub mail_poll: Duration,
    /// How long mail may take to arrive after the child's turn settles, before the launch
    /// reports that the child finished without mailing.
    pub reply_grace: Duration,
}

impl Default for LaunchTuning {
    fn default() -> Self {
        Self {
            start_retry_interval: START_RETRY_INTERVAL,
            start_retry_window: START_RETRY_WINDOW,
            ready_poll: READY_POLL,
            quiet_window: QUIET_WINDOW,
            dialog_settle: DIALOG_SETTLE,
            idle_poll: IDLE_POLL,
            startup_grace: STARTUP_GRACE,
            mail_poll: MAIL_POLL,
            reply_grace: REPLY_GRACE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum LaunchWait {
    /// Wait for the first turn to settle, bounded by the wait cap.
    Turn,
    /// Return once the prompt has started a turn.
    None,
}

/// How the child's result gets back to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// The result is read off the child's screen: the wait excerpt. Default.
    #[default]
    Screen,
    /// The child mails its exact result to you. Needs a verified caller and a kind with the MCP server.
    Mail,
}

/// What to do with a startup dialog, such as Codex's hook review or a folder trust prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub enum Dialogs {
    /// Take the option that grants nothing. Default.
    #[default]
    Skip,
    /// Stop with agent_needs_input and type nothing.
    Surface,
    /// Take the permissive option. Only when the user asked for it.
    Trust,
}

/// Most dialogs handled in one launch.
const MAX_DIALOGS: usize = 3;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct LaunchParams {
    /// Herdr agent kind, e.g. claude, codex. MCP is handed only to claude and codex; other kinds start without it.
    pub kind: String,
    /// Unique agent name. Refused while an agent with this name is live.
    pub name: String,
    /// The first prompt. Without it the agent is started and left idle.
    pub instructions: Option<String>,
    /// Where to start it. Default: a new tab in the calling pane's workspace.
    #[serde(rename = "where")]
    pub placement: Option<Where>,
    /// Working directory for a new pane.
    pub cwd: Option<String>,
    /// Environment for a new pane.
    pub env: Option<HashMap<String, String>>,
    /// Name for a new tab, workspace or worktree.
    pub label: Option<String>,
    /// Give the child control tools. Default: read tools only.
    pub control: Option<bool>,
    /// turn waits for the first turn to settle; none (default) returns the turn token.
    pub wait: Option<LaunchWait>,
    /// screen (default) leaves the result on the child's screen. mail appends an instruction for the child to mail its exact final result to you: wait turn returns it as `reply` (and `reply: null` plus the excerpt if the child finishes without mailing), wait none returns `reply: "pending"` and the mail nudge wakes you. Needs a verified caller and a kind that gets this MCP server.
    pub reply: Option<Reply>,
    /// How long the agent may take to become ready, in milliseconds. Default 30000.
    pub start_timeout_ms: Option<u64>,
    /// Startup dialogs (hook review, folder trust) the agent shows before its prompt. skip (default) takes the option that grants nothing; surface stops with agent_needs_input; trust takes the permissive option, only when the user asked.
    pub dialogs: Option<Dialogs>,
    /// Permissions the child starts with: inherit (default, no flags), approval-required, auto-accept-edits or full-access. Never above the caller's own mode. Codex and claude only.
    pub runtime_mode: Option<RuntimeMode>,
    /// Lets full-access through when the caller's own mode is unknown. Only when the user asked.
    pub allow_full_access: Option<bool>,
}

pub async fn run(
    inner: &Inner,
    client_info_name: Option<&str>,
    cancel: CancellationToken,
    ticks: Option<UnboundedSender<Progress>>,
    params: LaunchParams,
) -> Result<Value, HerdrError> {
    let call_start = Instant::now();
    if params.name.trim().is_empty() || params.kind.trim().is_empty() {
        return Err(HerdrError::invalid_params(
            "kind and name must not be empty",
        ));
    }
    let wait = params.wait.unwrap_or(LaunchWait::None);
    match params.instructions.as_deref() {
        Some("") => return Err(HerdrError::invalid_params("instructions must not be empty")),
        None if wait == LaunchWait::Turn => {
            return Err(HerdrError::invalid_params(
                "wait turn needs instructions to start a turn",
            ));
        }
        _ => {}
    }
    let reply = params.reply.unwrap_or_default();
    if reply == Reply::Mail && params.instructions.is_none() {
        return Err(HerdrError::invalid_params(
            "reply mail needs instructions to start a turn",
        ));
    }
    let control = params.control.unwrap_or(false);
    if control && inner.mode != ToolMode::Control {
        return Err(HerdrError::tool(
            "control_refused",
            "this bridge is in read mode, so it cannot hand control tools to a child",
        ));
    }
    let requested_mode = params.runtime_mode.unwrap_or_default();
    let mode_args = match requested_mode.level() {
        None => Vec::new(),
        Some(level) => runtime_args(&params.kind, level).ok_or_else(|| {
            HerdrError::tool(
                "runtime_mode_unsupported",
                format!(
                    "kind {:?} has no known permission flags, so runtime_mode {} cannot be applied; use inherit",
                    params.kind,
                    requested_mode.as_str()
                ),
            )
        })?,
    };
    let options = Options {
        remote: inner.is_remote(),
        cwd: params.cwd,
        env: params.env.unwrap_or_default(),
        label: params.label,
    };
    place::validate(params.placement.as_ref(), &options)?;

    let mut scope = Scope::open(inner)?;
    let (snapshot, resolution) = scope.snapshot_and_caller().await?;
    let client = scope.client();
    if snapshot
        .agents
        .iter()
        .any(|agent| agent.name.as_deref() == Some(params.name.as_str()))
    {
        return Err(HerdrError::tool(
            "name_taken",
            format!(
                "an agent named {:?} is live; pick another name",
                params.name
            ),
        ));
    }
    let mut mail_injection = None;
    let mail_plan = match reply {
        Reply::Screen => None,
        Reply::Mail => {
            require_verified_caller(&resolution)?;
            let found = injection(inner, &params.kind, control)?;
            let child_has_mail_tool = found.mcp != McpStatus::Unsupported.as_str();
            mail_injection = Some(found);
            Some(mail_plan(
                &params.kind,
                child_has_mail_tool,
                &snapshot,
                &resolution,
            )?)
        }
    };
    let caller_mode = match (&resolution.caller, resolution.caller_verified) {
        (Some(caller), true) if requested_mode != RuntimeMode::Inherit => client
            .pane_process_info(&caller.pane_id)
            .await
            .ok()
            .and_then(|info| runtime_mode::caller_level(&info)),
        _ => None,
    };
    runtime_mode::check_escalation(
        requested_mode,
        caller_mode,
        params.allow_full_access.unwrap_or(false),
    )?;
    let placed = place::place(client, &resolution, params.placement.as_ref(), &options).await?;
    let mut session = LaunchSession::new(client, placed, &params.name);

    let injected = match mail_injection {
        Some(found) => found,
        None => match injection(inner, &params.kind, control) {
            Ok(found) => found,
            Err(err) => return Err(session.fail_and_clean_up(err).await),
        },
    };
    let mut start = AgentStartParams::new(&params.name, &params.kind, &session.placed().pane_id);
    start.args = mode_args.into_iter().chain(injected.args).collect();
    let cap = inner.profile.wait_cap(client_info_name);
    let (timeout, cap_note) = clamp_timeout(params.start_timeout_ms, cap, DEFAULT_TIMEOUT);
    let started = session
        .race(
            &cancel,
            start_agent(client, &inner.launch, &session.placed().pane_id, &start),
        )
        .await?;
    if let Err(err) = started {
        return Err(session.fail_and_clean_up(err).await);
    }
    session.advance(Phase::Started);

    if let Err(err) = provenance::tag_agent(
        client,
        &session.placed().pane_id,
        provenance::parent_value(&resolution),
    )
    .await
    {
        let failure = HerdrError::detailed(
            "provenance_failed",
            format!(
                "the agent started but could not be tagged with its launcher ({err}); the pane is kept unless this call created it"
            ),
            session.details(),
        );
        return Err(session.fail_and_clean_up(failure).await);
    }

    let ready = session
        .race(
            &cancel,
            await_ready(&session, &inner.launch, &params.kind, timeout),
        )
        .await?;
    if let Err(failure) = ready {
        return Err(match failure {
            NotReady::Cleanup(err) => session.fail_and_clean_up(err).await,
            NotReady::Keep(err) => session.fail_keep(err),
        });
    }

    let handled = handle_dialogs(
        client,
        &inner.launch,
        &session,
        &params.kind,
        params.dialogs.unwrap_or_default(),
    )
    .await?;

    let mut out = session.details();
    if !handled.is_empty() {
        out["dialogs"] = json!(
            handled
                .iter()
                .map(|(dialog, action)| json!({
                    "id": dialog.id, "action": action, "verified": dialog.verified,
                }))
                .collect::<Vec<_>>()
        );
        if params.kind == "codex"
            && handled
                .iter()
                .any(|(dialog, action)| dialog.id == "hook_review" && *action == "skipped")
        {
            add_note(
                &mut out,
                "Codex hooks were skipped for this child, so Herdr resume hooks will not run for it. Trust the hooks once in Codex to stop this dialog.",
            );
        }
    }
    out["kind"] = json!(params.kind);
    out["runtime_mode"] = json!(requested_mode.as_str());
    out["caller_runtime_mode"] = json!(runtime_mode::describe(caller_mode));
    out["mcp"] = json!(injected.mcp);
    out["child_tools"] = json!(injected.child_tools.as_str());
    if let Some(note) = &injected.note {
        add_note(&mut out, note);
    }
    // The default timeout is the bridge's own choice, so only a timeout the caller asked for is noted.
    if let Some(note) = cap_note.filter(|_| params.start_timeout_ms.is_some()) {
        add_note(&mut out, &note);
    }
    out["turn"] = Value::Null;
    let Some(base_instructions) = params.instructions.as_deref() else {
        return Ok(out);
    };
    let with_reply;
    let instructions = match &mail_plan {
        Some(plan) => {
            with_reply = format!("{base_instructions}\n\n{}", reply_paragraph(&plan.parent));
            with_reply.as_str()
        }
        None => base_instructions,
    };

    let agent = gated_prompt(client, &session.placed().pane_id, instructions)
        .await
        .map_err(|err| session.fail_prompt(err))?;
    let turn = turn_for(client, &agent).await;
    session.advance(Phase::Prompted);
    out["phase"] = json!(Phase::Prompted.as_str());
    out["status"] = json!(agent.agent_status);
    out["turn"] = json!(turn);

    if mail_plan.is_some() && wait == LaunchWait::None {
        out["reply"] = json!("pending");
        add_note(
            &mut out,
            "The child will mail its result to you; end your turn, and the mail nudge will wake you.",
        );
    }
    let mailbox = inner.mailbox();
    let filter = SenderFilter::new([session.placed().pane_id.clone(), params.name.clone()]);
    let Some(turn) = turn else {
        add_note(
            &mut out,
            "the prompt was accepted but the agent is neither working nor blocked, so there is no turn to wait on; read the pane",
        );
        if let (Some(plan), LaunchWait::Turn) = (&mail_plan, wait) {
            let mailed = take_reply(&mailbox, &plan.inbox, &filter)?;
            set_reply(&mut out, mailed);
        }
        return Ok(out);
    };
    if wait != LaunchWait::Turn {
        return Ok(out);
    }
    session.advance(Phase::Waiting);
    out["phase"] = json!(Phase::Waiting.as_str());
    if turn.from == TurnFrom::Blocked {
        out["matched"] = json!(true);
        out["timed_out"] = json!(false);
        out["state_change_seq"] = json!(agent.state_change_seq);
        out["completion_seq"] = json!(agent.completion_seq);
        if mail_plan.is_some() {
            out["reply"] = Value::Null;
        }
        out["excerpt"] = json!(
            read_text(client, &session.placed().pane_id)
                .await
                .map(|text| tail(&text))
                .unwrap_or_default()
        );
        return Ok(out);
    }

    let remaining = cap.saturating_sub(call_start.elapsed());
    if remaining.is_zero() {
        out["timed_out"] = json!(true);
        add_note(
            &mut out,
            "the wait cap was used up before the turn could be waited on; call herdr_wait with the turn",
        );
        return Ok(out);
    }
    session.attach_turn(json!(turn));
    let result = match &mail_plan {
        Some(plan) => {
            let details = session.details();
            await_reply(ReplyWait {
                client,
                mailbox: &mailbox,
                wait: &inner.wait,
                mail_poll: inner.launch.mail_poll,
                reply_grace: inner.launch.reply_grace,
                plan,
                filter: &filter,
                turn: turn.clone(),
                name: &params.name,
                remaining,
                ticks,
                cancel: &cancel,
                details: &details,
            })
            .await
            .map(|outcome| outcome.to_json())
        }
        None => session
            .race(
                &cancel,
                wait::wait(
                    client,
                    &inner.wait,
                    Request {
                        pane: turn.pane.clone(),
                        condition: Condition::Turn(turn.clone()),
                        cap: remaining,
                    },
                    ticks,
                ),
            )
            .await?
            .map(|outcome| outcome.to_json()),
    };
    match result {
        Ok(fields) => {
            merge_fields(&mut out, fields);
            Ok(out)
        }
        Err(err) => Err(session.fail_keep(err)),
    }
}

fn set_reply(out: &mut Value, message: Option<Message>) {
    match message {
        Some(message) => out["reply"] = reply_json(&message),
        None => {
            out["reply"] = Value::Null;
            add_note(out, NO_MAIL_NOTE);
        }
    }
}

/// The dialog of `kind` on the pane's visible screen and the screen's tail, or `None` when the
/// screen shows none or cannot be read.
pub(crate) async fn visible_dialog(
    client: &Client,
    pane: &str,
    kind: &str,
) -> Option<(&'static Dialog, String)> {
    let mut params = AgentReadParams::new(pane);
    params.source = ReadSource::Visible;
    let read = client.agent_read(&params).await.ok()?;
    match_dialog(Some(kind), &read.text).map(|dialog| (dialog, tail(&read.text)))
}

fn needs_input(session: &LaunchSession, dialog: &Dialog, excerpt: &str, why: &str) -> HerdrError {
    let name = session.name();
    let mut details = session.details();
    details["dialog"] = json!({"id": dialog.id, "excerpt": excerpt});
    HerdrError::detailed(
        "agent_needs_input",
        format!(
            "{name} shows a startup dialog ({}) {why}. The pane is kept: read it, then answer with herdr_send and answer_blocked, or ask the user",
            dialog.id
        ),
        details,
    )
}

/// Answers the startup dialogs on the visible screen, at most `MAX_DIALOGS`, waiting after each
/// answer for the screen to move on. Returns each dialog with the action taken.
async fn handle_dialogs(
    client: &Client,
    tuning: &LaunchTuning,
    session: &LaunchSession<'_>,
    kind: &str,
    mode: Dialogs,
) -> Result<Vec<(&'static Dialog, &'static str)>, HerdrError> {
    let pane = session.placed().pane_id.as_str();
    let mut handled = Vec::new();
    let mut current = visible_dialog(client, pane, kind).await;
    while let Some((dialog, excerpt)) = current {
        if mode == Dialogs::Surface {
            return Err(needs_input(
                session,
                dialog,
                &excerpt,
                "and dialogs is surface",
            ));
        }
        if handled.len() >= MAX_DIALOGS {
            return Err(needs_input(
                session,
                dialog,
                &excerpt,
                "after too many dialogs",
            ));
        }
        let (keys, action) = match mode {
            Dialogs::Trust => (dialog.trust_keys, "trusted"),
            _ => (dialog.skip_keys, "skipped"),
        };
        let keys: Vec<String> = keys.iter().map(|key| key.to_string()).collect();
        client.agent_send_keys(pane, &keys).await?;
        handled.push((dialog, action));

        let deadline = Instant::now() + tuning.dialog_settle;
        loop {
            sleep(tuning.ready_poll).await;
            current = visible_dialog(client, pane, kind).await;
            match &current {
                Some((same, _)) if same.id == dialog.id => {}
                _ => break,
            }
            if Instant::now() >= deadline {
                let excerpt = current.map(|(_, excerpt)| excerpt).unwrap_or_default();
                return Err(needs_input(
                    session,
                    dialog,
                    &excerpt,
                    "that is still on screen after it was answered",
                ));
            }
        }
    }
    Ok(handled)
}

/// What the child is handed: extra start args, how it reaches the bridge, and its tool mode.
struct Injection {
    args: Vec<String>,
    mcp: &'static str,
    child_tools: ToolMode,
    note: Option<String>,
}

fn add_note(out: &mut Value, note: &str) {
    out["note"] = match out["note"].as_str() {
        Some(existing) => json!(format!("{existing}; {note}")),
        None => json!(note),
    };
}

fn merge_fields(out: &mut Value, fields: Value) {
    let Value::Object(fields) = fields else {
        return;
    };
    for (key, value) in fields {
        if key == "note" {
            if let Some(note) = value.as_str() {
                add_note(out, note);
            }
        } else {
            out[key] = value;
        }
    }
}

/// With no user-scope entry of ours, the child gets per-launch MCP args carrying `--tools read`
/// unless the call asked for control. With one, Codex gets a `-c` override of just the tool mode
/// when it differs from the request. Claude cannot be overridden per launch, so the global mode
/// is reported and a mismatch noted.
fn injection(inner: &Inner, kind: &str, control: bool) -> Result<Injection, HerdrError> {
    let requested = ToolMode::from_control(control);
    let status = mcp_status_for(inner.install_env.as_ref(), kind);
    match status {
        McpStatus::Injectable => {
            let bin = bridge_binary(inner)?;
            Ok(match mcp_args(kind, &bin, requested) {
                Some(args) => Injection {
                    args,
                    mcp: McpStatus::Injected.as_str(),
                    child_tools: requested,
                    note: None,
                },
                None => unsupported(),
            })
        }
        McpStatus::Global => {
            let global = inner
                .install_env
                .as_ref()
                .and_then(|env| global_mode(env, kind))
                .unwrap_or(ToolMode::Read);
            let mut injection = Injection {
                args: Vec::new(),
                mcp: status.as_str(),
                child_tools: global,
                note: None,
            };
            if global == requested {
                return Ok(injection);
            }
            match mode_args(kind, requested) {
                Some(args) => {
                    injection.args = args;
                    injection.child_tools = requested;
                }
                None => {
                    injection.note = Some(format!(
                        "the global herdr entry gives the child {} tools and cannot be overridden per launch, so the requested {} tools were not applied",
                        global.as_str(),
                        requested.as_str()
                    ));
                }
            }
            Ok(injection)
        }
        McpStatus::Injected | McpStatus::Unsupported => Ok(unsupported()),
    }
}

/// No MCP is handed to the child, so it has no control tools.
fn unsupported() -> Injection {
    Injection {
        args: Vec::new(),
        mcp: McpStatus::Unsupported.as_str(),
        child_tools: ToolMode::Read,
        note: None,
    }
}

fn bridge_binary(inner: &Inner) -> Result<String, HerdrError> {
    let shim: Option<PathBuf> = match &inner.install_env {
        Some(env) => Some(env.shim_path()),
        None => std::env::var_os("HOME").map(|home| shim_path_in(Path::new(&home))),
    };
    if let Some(shim) = shim.filter(|shim| shim.exists()) {
        return Ok(shim.display().to_string());
    }
    let exe = match &inner.install_env {
        Some(env) => Ok(env.exe.clone()),
        None => std::env::current_exe(),
    };
    exe.map(|exe| exe.display().to_string()).map_err(|err| {
        HerdrError::tool(
            "bridge_binary_unknown",
            format!("cannot locate this binary to hand to the child: {err}"),
        )
    })
}

async fn terminal_of(client: &Client, pane: &str) -> Option<String> {
    let snapshot = client.session_snapshot().await.ok()?;
    snapshot
        .panes
        .into_iter()
        .find(|candidate| candidate.pane_id == pane)
        .map(|candidate| candidate.terminal_id)
}

/// A fresh pane reports `agent_pane_busy` while its shell initialises. Retry while
/// the shell is in front and the terminal is unchanged.
async fn start_agent(
    client: &Client,
    tuning: &LaunchTuning,
    pane: &str,
    params: &AgentStartParams,
) -> Result<(), HerdrError> {
    let deadline = Instant::now() + tuning.start_retry_window;
    let mut terminal: Option<String> = None;
    loop {
        let busy = match client.agent_start(params).await {
            Ok(_) => return Ok(()),
            Err(
                err @ HerdrError::Api {
                    code: ApiErrorCode::AgentPaneBusy,
                    ..
                },
            ) => err,
            Err(err) => return Err(err),
        };
        if Instant::now() >= deadline {
            return Err(busy);
        }
        let (info, current) =
            tokio::join!(client.pane_process_info(pane), terminal_of(client, pane));
        let initialising = info.is_ok_and(|info| info.shell_in_front_strict());
        let unchanged = match (&terminal, &current) {
            (Some(was), Some(now)) => was == now,
            (None, Some(_)) => true,
            _ => false,
        };
        if !initialising || !unchanged {
            return Err(busy);
        }
        terminal = current;
        sleep(tuning.start_retry_interval).await;
    }
}

enum NotReady {
    Cleanup(HerdrError),
    Keep(HerdrError),
}

/// Readiness plus the probed quiet window: after first ready, the agent must show no
/// `state_change_seq` change for `quiet_window` before it is prompted.
async fn await_ready(
    session: &LaunchSession<'_>,
    tuning: &LaunchTuning,
    kind: &str,
    timeout: Duration,
) -> Result<AgentInfo, NotReady> {
    let client = session.client();
    let name = session.name();
    let deadline = Instant::now() + timeout;
    let pane = session.placed().pane_id.as_str();
    let mut seen_ready = false;
    let mut last_seq = 0;
    let mut quiet_since = Instant::now();
    let started = quiet_since;
    let mut seen_group = false;
    loop {
        let agent = client.agent_get(pane).await.map_err(NotReady::Cleanup)?;
        if agent.agent_status == AgentStatus::Blocked {
            return Err(NotReady::Keep(HerdrError::tool(
                "agent_not_ready",
                format!(
                    "{} is blocked during startup, probably on a trust or login prompt. The pane is kept: read it, answer it, then continue with herdr_send",
                    name
                ),
            )));
        }
        if agent.launch_pending
            && let Ok(info) = client.pane_process_info(pane).await
        {
            // A non-shell foreground group means the typed command has exec'd.
            if info.left_shell_group().is_some() {
                seen_group = true;
            } else if info.shell_in_front_strict()
                && (seen_group || started.elapsed() >= tuning.startup_grace)
            {
                let mut details = session.details();
                details["cleaned_up"] = json!(session.placed().created.is_some());
                return Err(NotReady::Cleanup(HerdrError::detailed(
                    "agent_exited",
                    format!(
                        "{kind} exited before it was ready; check the agent's arguments and login"
                    ),
                    details,
                )));
            }
        }

        let now = Instant::now();
        if seen_ready && agent.state_change_seq != last_seq {
            quiet_since = now;
        }
        last_seq = agent.state_change_seq;
        if is_ready(Some(kind), &agent) {
            if !seen_ready {
                seen_ready = true;
                quiet_since = now;
            }
            if now.duration_since(quiet_since) >= tuning.quiet_window {
                return Ok(agent);
            }
        }

        if now >= deadline {
            // One agent.get after a readiness timeout releases the name Herdr reserved for the start.
            let _ = client.agent_get(pane).await;
            return Err(NotReady::Keep(HerdrError::tool(
                "agent_not_ready",
                format!(
                    "{} was not ready after {} ms. The pane is kept: read it, then continue with herdr_send or close it",
                    name,
                    timeout.as_millis()
                ),
            )));
        }
        sleep(
            tuning
                .ready_poll
                .min(deadline.saturating_duration_since(now)),
        )
        .await;
    }
}
