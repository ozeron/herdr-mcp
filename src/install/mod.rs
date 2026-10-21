pub mod claude;
pub mod codex;
pub mod shim;
pub mod status;

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use thiserror::Error;

use self::shim::ShimState;
use self::status::{Classification, State, classify};
use crate::cli::ToolMode;
use crate::env::env_nonempty;
use crate::profile::ClientKind;

const KINDS: [ClientKind; 2] = [ClientKind::Claude, ClientKind::Codex];

pub const DISCLOSURE_OUTPUT: &str = "Pane output reaches your model provider: herdr_read sends any pane's output, human shells included, to the model of the agent that calls it.";
pub const DISCLOSURE_SESSIONS: &str = "Sessions outside Herdr reach the default Herdr session: every Claude and Codex session will list a herdr server and connect to it whenever Herdr is running.";
pub const DISCLOSURE_READ: &str = "Read mode registers discovery, read and wait tools only. Pass --control to add tools that run commands and type into panes.";
pub const DISCLOSURE_CONTROL: &str = "Control grants shell authority: any connected agent can run commands, type into and close panes across the Herdr session with your authority. Client sandboxes and approval modes do not contain it.";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct InstallError(String);

impl InstallError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The stable shim location under `home`.
pub fn shim_path_in(home: &Path) -> PathBuf {
    home.join(".local/bin").join(shim::SHIM_NAME)
}

#[derive(Debug, Clone)]
pub struct InstallEnv {
    pub home: PathBuf,
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub path: OsString,
    pub exe: PathBuf,
}

impl InstallEnv {
    pub fn from_process() -> Result<Self, InstallError> {
        let var = env_nonempty;
        let home = var("HOME").ok_or_else(|| InstallError::new("HOME is not set"))?;
        let exe = std::env::current_exe()
            .and_then(std::path::absolute)
            .map_err(|err| InstallError::new(format!("cannot locate this binary: {err}")))?;
        Ok(Self {
            home: PathBuf::from(home),
            claude_config_dir: var("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            codex_home: var("CODEX_HOME").map(PathBuf::from),
            path: var("PATH").unwrap_or_default(),
            exe,
        })
    }

    pub fn shim_path(&self) -> PathBuf {
        shim_path_in(&self.home)
    }

    fn find(&self, program: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command.env("HOME", &self.home).env("PATH", &self.path);
        match &self.claude_config_dir {
            Some(dir) => command.env("CLAUDE_CONFIG_DIR", dir),
            None => command.env_remove("CLAUDE_CONFIG_DIR"),
        };
        match &self.codex_home {
            Some(dir) => command.env("CODEX_HOME", dir),
            None => command.env_remove("CODEX_HOME"),
        };
        command
    }
}

fn run_capture(env: &InstallEnv, program: &str, args: &[&str]) -> Result<Output, InstallError> {
    let path = env
        .find(program)
        .ok_or_else(|| InstallError::new(format!("{program} not found on PATH")))?;
    env.command(&path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| InstallError::new(format!("{program}: {err}")))
}

pub(crate) fn run_cli(env: &InstallEnv, program: &str, args: &[&str]) -> Result<(), InstallError> {
    let output = run_capture(env, program, args)?;
    if output.status.success() {
        return Ok(());
    }
    Err(InstallError::new(format!(
        "`{program} {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Status,
    Remove,
    RelinkShim,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub control: bool,
    pub action: Action,
    pub yes: bool,
}

pub struct Io<'a> {
    pub stdin_is_tty: bool,
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

pub fn run(options: &Options, env: &InstallEnv, io: &mut Io) -> i32 {
    match options.action {
        Action::Status => status_report(env, io),
        Action::Install => install(options, env, io),
        Action::Remove => remove(options, env, io),
        Action::RelinkShim => relink_shim(env, io),
    }
}

/// Runs as the plugin's last build step; it never touches client config.
fn relink_shim(env: &InstallEnv, io: &mut Io) -> i32 {
    let path = env.shim_path();
    let line = match shim::relink(env) {
        Ok(shim::Relink::Relinked) => {
            format!("shim: relinked {} -> {}", path.display(), env.exe.display())
        }
        Ok(shim::Relink::Current) => "shim: current".to_owned(),
        Ok(shim::Relink::NotInstalled) => "shim: not installed; nothing to relink".to_owned(),
        Ok(shim::Relink::Left) => format!(
            "shim: left as is; {} does not point at a Herdr plugin build",
            path.display()
        ),
        Err(err) => return fail(io, err),
    };
    let _ = writeln!(io.out, "{line}");
    0
}

fn fail(io: &mut Io, message: impl std::fmt::Display) -> i32 {
    let _ = writeln!(io.err, "herdr-mcp: {message}");
    1
}

fn describe(found: &Classification) -> String {
    let mut details = Vec::new();
    if let Some(mode) = found.mode {
        details.push(format!("mode: {}", mode.as_str()));
    }
    details.extend(found.drift.iter().map(|drift| drift.as_str().to_owned()));
    match (found.state, details.is_empty()) {
        (State::Absent | State::Foreign, _) | (_, true) => found.state.as_str().to_owned(),
        _ => format!("{} ({})", found.state.as_str(), details.join("; ")),
    }
}

fn status_report(env: &InstallEnv, io: &mut Io) -> i32 {
    for kind in KINDS {
        let name = kind.as_str();
        match classify(env, kind, None) {
            Ok(found) => {
                let _ = writeln!(io.out, "{name}: {}", describe(&found));
                let _ = writeln!(io.out, "  config: {}", found.config_path.display());
                for warning in &found.warnings {
                    let _ = writeln!(io.out, "  warning: {warning}");
                }
            }
            Err(err) => {
                let _ = writeln!(io.out, "{name}: error ({err})");
            }
        }
    }
    let shim_state = match shim::inspect(env) {
        ShimState::Missing => "missing",
        ShimState::Current => "ok",
        ShimState::Moved => "points at another binary",
        ShimState::Dangling => "dangling",
        ShimState::Foreign => "foreign file",
    };
    let _ = writeln!(io.out, "shim: {} ({shim_state})", env.shim_path().display());
    integrations_report(env, io);
    0
}

fn integration_states(env: &InstallEnv) -> Option<Vec<(ClientKind, bool)>> {
    let output = run_capture(env, "herdr", &["integration", "status"])
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Some(
        KINDS
            .into_iter()
            .map(|kind| {
                let prefix = format!("{}:", kind.as_str());
                let installed = text
                    .lines()
                    .find_map(|line| line.strip_prefix(prefix.as_str()))
                    .is_some_and(|rest| !rest.trim_start().starts_with("not installed"));
                (kind, installed)
            })
            .collect(),
    )
}

fn integrations_report(env: &InstallEnv, io: &mut Io) {
    let Some(states) = integration_states(env) else {
        let _ = writeln!(
            io.out,
            "resume-on-restore hooks: unknown (`herdr integration status` unavailable)"
        );
        return;
    };
    let summary: Vec<String> = states
        .iter()
        .map(|(kind, installed)| {
            let state = if *installed {
                "installed"
            } else {
                "not installed"
            };
            format!("{} {state}", kind.as_str())
        })
        .collect();
    let _ = writeln!(io.out, "resume-on-restore hooks: {}", summary.join(", "));
    for (kind, installed) in states {
        if !installed {
            let _ = writeln!(
                io.out,
                "  agents of this kind are not resumed after a Herdr restart until you run `herdr integration install {}`; this installer never runs it",
                kind.as_str()
            );
        }
    }
}

fn confirm(options: &Options, io: &mut Io) -> Option<i32> {
    if options.yes {
        return None;
    }
    if !io.stdin_is_tty {
        let _ = writeln!(
            io.err,
            "herdr-mcp: stdin is not a terminal; nothing was written. Re-run with --yes to apply this plan."
        );
        return Some(2);
    }
    let _ = write!(io.out, "Proceed? [y/N] ");
    let _ = io.out.flush();
    let mut line = String::new();
    let _ = io.input.read_line(&mut line);
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        None
    } else {
        let _ = writeln!(io.out, "Aborted; nothing was written.");
        Some(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Unchanged,
    Add,
    Rewrite,
    RelinkOnly,
}

struct Target {
    kind: ClientKind,
    found: Classification,
    step: Step,
}

fn step_for(found: &Classification) -> Step {
    match found.state {
        State::Absent => Step::Add,
        State::Ours | State::Foreign => Step::Unchanged,
        State::ShimBroken => Step::RelinkOnly,
        State::Drifted if found.drift.iter().any(|drift| drift.needs_entry_write()) => {
            Step::Rewrite
        }
        State::Drifted => Step::RelinkOnly,
    }
}

fn print_disclosure(io: &mut Io, control: bool) {
    let _ = writeln!(io.out, "What this exposes:");
    let _ = writeln!(io.out, "  - {DISCLOSURE_OUTPUT}");
    let _ = writeln!(io.out, "  - {DISCLOSURE_SESSIONS}");
    let third = if control {
        DISCLOSURE_CONTROL
    } else {
        DISCLOSURE_READ
    };
    let _ = writeln!(io.out, "  - {third}");
}

fn install(options: &Options, env: &InstallEnv, io: &mut Io) -> i32 {
    let mode = ToolMode::from_control(options.control);
    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    for kind in KINDS {
        if env.find(kind.as_str()).is_none() {
            skipped.push(kind);
            continue;
        }
        match classify(env, kind, Some(mode)) {
            Ok(found) => {
                let step = step_for(&found);
                targets.push(Target { kind, found, step });
            }
            Err(err) => return fail(io, format!("{}: {err}", kind.as_str())),
        }
    }
    if targets.is_empty() {
        return fail(
            io,
            "neither claude nor codex found on PATH; nothing to install",
        );
    }
    let shim_state = shim::inspect(env);
    if shim_state == ShimState::Foreign {
        return fail(
            io,
            format!(
                "{} exists and is not a herdr-mcp link; refusing to touch it. Nothing was changed.",
                env.shim_path().display()
            ),
        );
    }
    for target in &targets {
        if target.found.state == State::Foreign {
            return fail(
                io,
                format!(
                    "{}: foreign `herdr` entry in {} points elsewhere; refusing to overwrite it. Nothing was changed.",
                    target.kind.as_str(),
                    target.found.config_path.display()
                ),
            );
        }
    }

    let _ = writeln!(io.out, "herdr-mcp install plan (tools: {})", mode.as_str());
    let link_shim = shim_state != ShimState::Current;
    let shim_action = if link_shim { "link" } else { "keep" };
    let _ = writeln!(
        io.out,
        "  shim: {shim_action} {} -> {}",
        env.shim_path().display(),
        env.exe.display()
    );
    for target in &targets {
        let action = match target.step {
            Step::Unchanged => "unchanged",
            Step::Add => "add entry",
            Step::Rewrite => "rewrite entry",
            Step::RelinkOnly => "relink shim only",
        };
        let _ = writeln!(
            io.out,
            "  {}: {action} in {} ({})",
            target.kind.as_str(),
            target.found.config_path.display(),
            describe(&target.found)
        );
    }
    for kind in &skipped {
        let _ = writeln!(
            io.out,
            "  {}: skipped, CLI not found on PATH",
            kind.as_str()
        );
    }
    print_disclosure(io, options.control);

    if !link_shim && targets.iter().all(|target| target.step == Step::Unchanged) {
        let _ = writeln!(io.out, "Already installed; nothing to do.");
        return 0;
    }
    if let Some(code) = confirm(options, io) {
        return code;
    }

    if link_shim && let Err(err) = shim::link(env) {
        return fail(io, err);
    }
    for target in &targets {
        if let Err(err) = apply(env, target, mode) {
            return fail(io, format!("{}: {err}", target.kind.as_str()));
        }
    }
    let mut code = 0;
    for target in &targets {
        match classify(env, target.kind, Some(mode)) {
            Ok(found) => {
                let _ = writeln!(io.out, "{}: {}", target.kind.as_str(), describe(&found));
                if found.state != State::Ours {
                    code = 1;
                }
            }
            Err(err) => return fail(io, err),
        }
    }
    code
}

fn apply(env: &InstallEnv, target: &Target, mode: ToolMode) -> Result<(), InstallError> {
    match (target.kind, target.step) {
        (_, Step::Unchanged | Step::RelinkOnly) => Ok(()),
        (ClientKind::Claude, Step::Add) => claude::add(env, mode),
        (ClientKind::Claude, Step::Rewrite) => {
            claude::remove(env)?;
            claude::add(env, mode)
        }
        (ClientKind::Codex, Step::Add | Step::Rewrite) => codex::add(env, mode),
    }
}

fn remove(options: &Options, env: &InstallEnv, io: &mut Io) -> i32 {
    let mut doomed = Vec::new();
    let mut kept_ours = false;
    let _ = writeln!(io.out, "herdr-mcp remove plan");
    for kind in KINDS {
        let name = kind.as_str();
        let found = match classify(env, kind, None) {
            Ok(found) => found,
            Err(err) => return fail(io, format!("{name}: {err}")),
        };
        match found.state {
            State::Absent => {}
            State::Foreign => {
                let _ = writeln!(
                    io.out,
                    "  {name}: foreign `herdr` entry in {} stays",
                    found.config_path.display()
                );
            }
            _ if env.find(name).is_none() => {
                kept_ours = true;
                let _ = writeln!(io.out, "  {name}: entry stays, CLI not found on PATH");
            }
            _ => {
                let _ = writeln!(
                    io.out,
                    "  {name}: remove entry in {}",
                    found.config_path.display()
                );
                doomed.push(kind);
            }
        }
    }
    let shim_state = shim::inspect(env);
    let drop_shim = !kept_ours
        && matches!(
            shim_state,
            ShimState::Current | ShimState::Moved | ShimState::Dangling
        );
    if drop_shim {
        let _ = writeln!(io.out, "  shim: remove {}", env.shim_path().display());
    }
    if doomed.is_empty() && !drop_shim {
        let _ = writeln!(io.out, "Nothing to remove.");
        return 0;
    }
    if let Some(code) = confirm(options, io) {
        return code;
    }
    for kind in doomed {
        let result = match kind {
            ClientKind::Claude => claude::remove(env),
            ClientKind::Codex => codex::remove(env),
        };
        if let Err(err) = result {
            return fail(io, format!("{}: {err}", kind.as_str()));
        }
    }
    if drop_shim && let Err(err) = shim::unlink(env) {
        return fail(io, err);
    }
    let _ = writeln!(io.out, "Removed.");
    0
}
