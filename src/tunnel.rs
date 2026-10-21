use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;

use crate::audit::{self, iso8601, unix_now};
use crate::env::{env_nonempty, env_nonempty_string};
use crate::install::{Io, shim_path_in};

pub const KEYCHAIN_SERVICE: &str = "herdr-mcp-tunnel";
pub const KEYCHAIN_ACCOUNT: &str = "runtime-key";
pub const KEY_ENV: &str = "CONTROL_PLANE_API_KEY";
const CLIENT: &str = "tunnel-client";
const FALLBACK_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

pub const LABEL: &str = "dev.herdr-mcp.tunnel";
const AGENT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
const LAUNCHCTL: &str = "/bin/launchctl";
const STATE_FILE: &str = "tunnel.state";
pub const CONFIRM_PHRASE: &str = "full access";

#[derive(Debug, Error)]
#[error("{message}")]
pub struct TunnelError {
    message: String,
    code: i32,
}

impl TunnelError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }

    /// The process exit code this error maps to.
    pub fn code(&self) -> i32 {
        self.code
    }
}

/// Runs `launchctl`. Tests swap it for one that records the arguments.
pub trait Runner: Send + Sync {
    /// Runs `program` with `args` and returns its exit code.
    fn run(&self, program: &Path, args: &[String]) -> std::io::Result<i32>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &Path, args: &[String]) -> std::io::Result<i32> {
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.code().unwrap_or(1))
    }
}

/// Stops the tunnel service from inside the bridge.
pub trait Stopper: Send + Sync {
    fn stop(&self);
}

pub struct ServiceStopper {
    env: TunnelEnv,
    runner: Arc<dyn Runner>,
}

impl Stopper for ServiceStopper {
    fn stop(&self) {
        if let Err(err) = stop_service(&self.env, self.runner.as_ref()) {
            eprintln!("herdr-mcp: cannot stop the tunnel: {err}");
        }
    }
}

/// What `herdr-mcp --tunnel` adds to the bridge.
#[derive(Clone)]
pub struct TunnelConfig {
    /// What `XDG_STATE_HOME` would be: the audit log lives under it.
    pub state_home: PathBuf,
    pub expires_at: Option<u64>,
    pub stopper: Arc<dyn Stopper>,
}

impl TunnelConfig {
    pub fn from_process(expires_at: Option<u64>) -> Result<Self, TunnelError> {
        let env = TunnelEnv::from_process()?;
        Ok(Self {
            state_home: env.state_home(),
            expires_at,
            stopper: Arc::new(ServiceStopper {
                env,
                runner: Arc::new(SystemRunner),
            }),
        })
    }
}

#[derive(Debug, Clone)]
pub struct TunnelEnv {
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub uid: u32,
    pub path: OsString,
    pub exe: PathBuf,
    pub api_key: Option<String>,
    pub fallback_dirs: Vec<PathBuf>,
}

impl TunnelEnv {
    pub fn from_process() -> Result<Self, TunnelError> {
        let home = env_nonempty("HOME").ok_or_else(|| TunnelError::new("HOME is not set"))?;
        let exe = std::env::current_exe()
            .and_then(std::path::absolute)
            .map_err(|err| TunnelError::new(format!("cannot locate this binary: {err}")))?;
        Ok(Self {
            home: PathBuf::from(home),
            xdg_config_home: env_nonempty("XDG_CONFIG_HOME").map(PathBuf::from),
            xdg_state_home: env_nonempty("XDG_STATE_HOME")
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute()),
            // SAFETY: getuid has no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            path: env_nonempty("PATH").unwrap_or_default(),
            exe,
            api_key: env_nonempty_string(KEY_ENV),
            fallback_dirs: FALLBACK_DIRS.iter().map(PathBuf::from).collect(),
        })
    }

    pub fn id_path(&self) -> PathBuf {
        let base = match &self.xdg_config_home {
            Some(dir) => dir.clone(),
            None => self.home.join(".config"),
        };
        base.join("herdr-mcp/tunnel")
    }

    pub fn state_home(&self) -> PathBuf {
        match &self.xdg_state_home {
            Some(dir) => dir.clone(),
            None => self.home.join(".local/state"),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.state_home().join("herdr-mcp")
    }

    pub fn audit_path(&self) -> PathBuf {
        audit::log_path(&self.state_home())
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir().join(STATE_FILE)
    }

    fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn launchctl(&self) -> PathBuf {
        self.on_path("launchctl")
            .unwrap_or_else(|| PathBuf::from(LAUNCHCTL))
    }

    fn on_path(&self, program: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    }

    fn find_client(&self) -> Option<PathBuf> {
        self.on_path(CLIENT).or_else(|| {
            self.fallback_dirs
                .iter()
                .map(|dir| dir.join(CLIENT))
                .find(|candidate| candidate.is_file())
        })
    }

    fn bridge(&self) -> PathBuf {
        let shim = shim_path_in(&self.home);
        if shim.exists() {
            shim
        } else {
            self.exe.clone()
        }
    }
}

/// The shape `tunnel-client` itself accepts: `tunnel_` plus 32 lowercase letters or digits.
pub fn validate_tunnel_id(raw: &str) -> Result<&str, TunnelError> {
    let id = raw.trim();
    match id.strip_prefix("tunnel_") {
        Some(rest)
            if rest.len() == 32
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) =>
        {
            Ok(id)
        }
        _ => Err(TunnelError::new(
            "a tunnel ID is tunnel_ followed by 32 lowercase letters or digits",
        )),
    }
}

fn quote(word: &str) -> String {
    if word.is_empty() || word.contains(char::is_whitespace) || word.contains(['"', '\'', '\\']) {
        format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        word.to_owned()
    }
}

/// What `tunnel run` was asked for.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub session: Option<String>,
    pub full: bool,
    /// Set only by `tunnel start --full`, after the typed disclosure.
    pub full_confirmed: bool,
    pub expires_at: Option<u64>,
}

fn mcp_command(bridge: &Path, options: &RunOptions) -> String {
    let tools = if options.full { "control" } else { "read" };
    let mut line = format!(
        "{} --tools {tools} --tunnel",
        quote(&bridge.display().to_string())
    );
    if let Some(at) = options.expires_at {
        line.push_str(&format!(" --expires-at {at}"));
    }
    if let Some(name) = &options.session {
        line.push_str(&format!(" --session {}", quote(name)));
    }
    line
}

/// The `tunnel-client` invocation. The key goes into the child's environment only.
pub fn build_command(
    client: &Path,
    tunnel_id: &str,
    bridge: &Path,
    options: &RunOptions,
    key: &str,
) -> Command {
    let mut command = Command::new(client);
    command
        .args(["run", "--control-plane.tunnel-id", tunnel_id])
        .arg("--mcp-command")
        .arg(mcp_command(bridge, options))
        .args(["--health.listen-addr", "127.0.0.1:0"])
        .env(KEY_ENV, key);
    command
}

fn read_key(env: &TunnelEnv) -> Result<String, TunnelError> {
    if let Some(key) = &env.api_key {
        return Ok(key.clone());
    }
    let missing = || {
        TunnelError::new(format!(
            "no runtime key: set {KEY_ENV} or run `herdr-mcp tunnel setup`"
        ))
    };
    let security = env.on_path("security").ok_or_else(missing)?;
    let output = Command::new(security)
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|err| TunnelError::new(format!("security: {err}")))?;
    let key = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !key.is_empty() {
        Ok(key)
    } else {
        Err(missing())
    }
}

fn read_tunnel_id(env: &TunnelEnv) -> Result<String, TunnelError> {
    let path = env.id_path();
    let text = fs::read_to_string(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            TunnelError::new("no tunnel ID saved: run `herdr-mcp tunnel setup`")
        } else {
            TunnelError::new(format!("{}: {err}", path.display()))
        }
    })?;
    validate_tunnel_id(&text)
        .map(str::to_owned)
        .map_err(|err| TunnelError::new(format!("{}: {err}", path.display())))
}

/// The `tunnel-client` binary, the saved tunnel ID and the runtime key, or what to do about the first one missing.
fn ready(env: &TunnelEnv) -> Result<(PathBuf, String, String), TunnelError> {
    let client = env.find_client().ok_or_else(|| {
        TunnelError::new(format!(
            "{CLIENT} not found. Install it with: brew install openai/tools/tunnel-client"
        ))
    })?;
    let id = read_tunnel_id(env)?;
    let key = read_key(env)?;
    Ok((client, id, key))
}

/// Checks everything `tunnel run` needs, prints the banner and returns the command to exec.
pub fn prepare_run(
    env: &TunnelEnv,
    options: &RunOptions,
    io: &mut Io,
) -> Result<Command, TunnelError> {
    if options.full && !options.full_confirmed {
        return Err(TunnelError::usage(
            "full access needs the typed disclosure: run `herdr-mcp tunnel start --full`",
        ));
    }
    let (client, id, key) = ready(env)?;
    if options.full {
        let _ = writeln!(
            io.out,
            "FULL ACCESS: ChatGPT can run commands and type into your panes while this runs.\nAudit log: {}",
            env.audit_path().display()
        );
    } else {
        let _ = writeln!(
            io.out,
            "ChatGPT can read your Herdr panes while this runs. Read tools only.\nClose this pane or press Ctrl-C to stop."
        );
    }
    let _ = io.out.flush();
    Ok(build_command(&client, &id, &env.bridge(), options, &key))
}

fn save_tunnel_id(env: &TunnelEnv, id: &str) -> Result<(), TunnelError> {
    let path = env.id_path();
    let fail = |err: std::io::Error| TunnelError::new(format!("{}: {err}", path.display()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(fail)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(fail)?;
    writeln!(file, "{id}").map_err(fail)
}

fn store_key(env: &TunnelEnv) -> Result<(), TunnelError> {
    let security = env
        .on_path("security")
        .ok_or_else(|| TunnelError::new("security not found; the Keychain is macOS only"))?;
    // `-w` last makes security prompt for the secret on the terminal itself.
    let status = Command::new(security)
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .status()
        .map_err(|err| TunnelError::new(format!("security: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(TunnelError::new("security did not store the key"))
    }
}

const CHATGPT_STEPS: &str = "Next, in ChatGPT:
  1. Plugins -> + -> Add custom MCP server
  2. Connection: Tunnel, then pick your tunnel
  3. Configure auth
  4. Accept the risk warning
  5. Create as a plugin
A workspace admin must allow custom MCP servers first.
Check who in the workspace can use the connector.
Then run `herdr-mcp tunnel start` (read tools) or `herdr-mcp tunnel start --full`.";

pub fn setup(env: &TunnelEnv, io: &mut Io) -> i32 {
    match try_setup(env, io) {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io.err, "herdr-mcp: {err}");
            1
        }
    }
}

fn try_setup(env: &TunnelEnv, io: &mut Io) -> Result<i32, TunnelError> {
    if !io.stdin_is_tty {
        let _ = writeln!(
            io.err,
            "herdr-mcp: stdin is not a terminal; run `herdr-mcp tunnel setup` in one."
        );
        return Ok(2);
    }
    let _ = write!(io.out, "Tunnel ID (tunnel_...): ");
    let _ = io.out.flush();
    let mut line = String::new();
    io.input
        .read_line(&mut line)
        .map_err(|err| TunnelError::new(format!("cannot read the tunnel ID: {err}")))?;
    let id = validate_tunnel_id(&line)?;
    let _ = writeln!(
        io.out,
        "Paste the runtime key (restricted, Tunnels Read+Use) when the Keychain asks. It goes straight to the Keychain, not through herdr-mcp."
    );
    let _ = io.out.flush();
    store_key(env)?;
    save_tunnel_id(env, id)?;
    let _ = writeln!(
        io.out,
        "Saved {}\n\n{CHATGPT_STEPS}",
        env.id_path().display()
    );
    Ok(0)
}

// ---- background service ----

const MACOS_ONLY: &str = "The background service is macOS only. `herdr-mcp tunnel run` works in the foreground on this platform.";

#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub full: bool,
    pub duration: Option<Duration>,
    pub session: Option<String>,
}

/// `30m`, `8h`, `2d` or `45s`.
pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    let bad =
        || format!("{raw:?} is not a duration; use a number and s, m, h or d, like 30m or 8h");
    let unit = raw.chars().last().ok_or_else(bad)?;
    let digits = &raw[..raw.len() - unit.len_utf8()];
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = digits.parse().map_err(|_| bad())?;
    match count.checked_mul(scale) {
        Some(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => Err(bad()),
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist(env: &TunnelEnv, run_args: &[String]) -> String {
    let mut args = vec![
        env.bridge().display().to_string(),
        "tunnel".into(),
        "run".into(),
    ];
    args.extend(run_args.iter().cloned());
    let program: String = args
        .iter()
        .map(|arg| format!("    <string>{}</string>\n", xml(arg)))
        .collect();
    let state = env.state_dir();
    let mut vars = vec![
        ("HOME", env.home.display().to_string()),
        ("PATH", AGENT_PATH.to_owned()),
    ];
    if let Some(dir) = &env.xdg_config_home {
        vars.push(("XDG_CONFIG_HOME", dir.display().to_string()));
    }
    if let Some(dir) = &env.xdg_state_home {
        vars.push(("XDG_STATE_HOME", dir.display().to_string()));
    }
    let vars: String = vars
        .iter()
        .map(|(name, value)| {
            format!(
                "    <key>{name}</key>\n    <string>{}</string>\n",
                xml(value)
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{program}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict>
{vars}  </dict>
</dict>
</plist>
"#,
        out = xml(&state.join("tunnel.out.log").display().to_string()),
        err = xml(&state.join("tunnel.err.log").display().to_string()),
    )
}

fn disclosure(env: &TunnelEnv) -> String {
    format!(
        "FULL ACCESS

ChatGPT will be able to:
  - run shell commands as you
  - type into any pane
  - launch and close agents in the active Herdr session, and switch sessions

Pane output goes to OpenAI.
Anyone who can use your ChatGPT connector gets this access.

To stop it:
  - run `herdr-mcp tunnel stop`
  - ask ChatGPT to call `herdr_tunnel_stop`
  - disable the plugin in ChatGPT
  - revoke the runtime key in the OpenAI Platform

Every call is logged to {}
",
        env.audit_path().display()
    )
}

fn confirm_full(env: &TunnelEnv, io: &mut Io) -> Result<(), TunnelError> {
    if !io.stdin_is_tty {
        return Err(TunnelError::usage(
            "full access needs a terminal for the typed disclosure; run `herdr-mcp tunnel start --full` in one",
        ));
    }
    let _ = write!(
        io.out,
        "{}\nType `{CONFIRM_PHRASE}` to continue: ",
        disclosure(env)
    );
    let _ = io.out.flush();
    let mut line = String::new();
    io.input
        .read_line(&mut line)
        .map_err(|err| TunnelError::new(format!("cannot read the answer: {err}")))?;
    if line.trim_end_matches(['\r', '\n']) == CONFIRM_PHRASE {
        Ok(())
    } else {
        Err(TunnelError::new(
            "not confirmed; nothing was started and nothing was written",
        ))
    }
}

fn private_dir(dir: &Path) -> Result<(), TunnelError> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(0o700)))
        .map_err(|err| TunnelError::new(format!("{}: {err}", dir.display())))
}

fn write_file(path: &Path, text: &str) -> Result<(), TunnelError> {
    let fail = |err: std::io::Error| TunnelError::new(format!("{}: {err}", path.display()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(fail)
}

fn launchctl(env: &TunnelEnv, runner: &dyn Runner, args: &[&str]) -> Result<i32, TunnelError> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    runner
        .run(&env.launchctl(), &args)
        .map_err(|err| TunnelError::new(format!("launchctl: {err}")))
}

fn agent_target(env: &TunnelEnv) -> String {
    format!("{}/{LABEL}", env.domain())
}

fn is_loaded(env: &TunnelEnv, runner: &dyn Runner) -> Result<bool, TunnelError> {
    Ok(launchctl(env, runner, &["print", &agent_target(env)])? == 0)
}

fn report(io: &mut Io, result: Result<i32, TunnelError>) -> i32 {
    match result {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io.err, "herdr-mcp: {err}");
            err.code()
        }
    }
}

pub fn start(
    env: &TunnelEnv,
    options: &StartOptions,
    runner: &dyn Runner,
    now: u64,
    io: &mut Io,
) -> i32 {
    if !cfg!(target_os = "macos") {
        let _ = writeln!(io.err, "herdr-mcp: {MACOS_ONLY}");
        return 1;
    }
    let result = try_start(env, options, runner, now, io);
    report(io, result)
}

fn try_start(
    env: &TunnelEnv,
    options: &StartOptions,
    runner: &dyn Runner,
    now: u64,
    io: &mut Io,
) -> Result<i32, TunnelError> {
    ready(env)?;
    if env.api_key.is_some() {
        let _ = writeln!(
            io.err,
            "note: the background service cannot see {KEY_ENV}; it reads the Keychain, so run `herdr-mcp tunnel setup` if you have not."
        );
    }
    if options.full {
        confirm_full(env, io)?;
    }
    let expires_at = options.duration.map(|d| now + d.as_secs());
    let mut run_args = Vec::new();
    if options.full {
        run_args.extend(["--full".to_owned(), "--full-confirmed".to_owned()]);
    }
    if let Some(at) = expires_at {
        run_args.extend(["--expires-at".to_owned(), at.to_string()]);
    }
    if let Some(name) = &options.session {
        run_args.extend(["--session".to_owned(), name.clone()]);
    }

    private_dir(&env.state_dir())?;
    let plist_path = env.plist_path();
    write_file(&plist_path, &plist(env, &run_args))?;

    let was_loaded = is_loaded(env, runner)?;
    if was_loaded {
        launchctl(env, runner, &["bootout", &agent_target(env)])?;
    }
    let plist_arg = plist_path.display().to_string();
    let mut code = launchctl(env, runner, &["bootstrap", &env.domain(), &plist_arg])?;
    // A bootout is asynchronous; give the old job a moment before giving up.
    for _ in 0..2 {
        if code == 0 || !was_loaded {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
        code = launchctl(env, runner, &["bootstrap", &env.domain(), &plist_arg])?;
    }
    if code != 0 {
        let _ = fs::remove_file(&plist_path);
        return Err(TunnelError::new(format!(
            "launchctl bootstrap failed (exit {code}); nothing is running"
        )));
    }

    let mode = if options.full { "full" } else { "read" };
    let state = json!({
        "mode": mode,
        "started_at": now,
        "expires_at": expires_at,
        "session": options.session,
    });
    write_file(&env.state_file(), &state.to_string())?;
    let expiry = match expires_at {
        Some(at) => format!("stops at {}", iso8601(at)),
        None => "no time limit".to_owned(),
    };
    let _ = writeln!(
        io.out,
        "Tunnel started in the background ({mode} tools, {expiry}).\nStatus: herdr-mcp tunnel status\nStop:   herdr-mcp tunnel stop\nAudit log: {}",
        env.audit_path().display()
    );
    Ok(0)
}

/// Removes the plist and the state file, then boots the agent out. The bridge calls this from
/// inside the agent, and the bootout ends that process, so the files go first.
pub fn stop_service(env: &TunnelEnv, runner: &dyn Runner) -> Result<bool, TunnelError> {
    let loaded = is_loaded(env, runner)?;
    for path in [env.plist_path(), env.state_file()] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(TunnelError::new(format!("{}: {err}", path.display()))),
        }
    }
    if loaded {
        let target = agent_target(env);
        let code = launchctl(env, runner, &["bootout", &target])?;
        if code != 0 {
            return Err(TunnelError::new(format!(
                "launchctl bootout failed (exit {code}); run `launchctl bootout {target}`"
            )));
        }
    }
    Ok(loaded)
}

pub fn stop(env: &TunnelEnv, runner: &dyn Runner, io: &mut Io) -> i32 {
    if !cfg!(target_os = "macos") {
        let _ = writeln!(io.err, "herdr-mcp: {MACOS_ONLY}");
        return 1;
    }
    let result = stop_service(env, runner).map(|was_loaded| {
        let _ = writeln!(
            io.out,
            "{}",
            if was_loaded {
                "Tunnel stopped."
            } else {
                "Tunnel not running."
            }
        );
        0
    });
    report(io, result)
}

fn tail(path: &Path, count: usize) -> Option<Vec<String>> {
    let text = fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(count);
    Some(
        lines[skip..]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    )
}

pub fn status(env: &TunnelEnv, runner: &dyn Runner, io: &mut Io) -> i32 {
    if !cfg!(target_os = "macos") {
        let _ = writeln!(io.err, "herdr-mcp: {MACOS_ONLY}");
        return 1;
    }
    let result = try_status(env, runner, io);
    report(io, result)
}

fn try_status(env: &TunnelEnv, runner: &dyn Runner, io: &mut Io) -> Result<i32, TunnelError> {
    let loaded = is_loaded(env, runner)?;
    let state: Option<Value> = fs::read_to_string(env.state_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let _ = writeln!(
        io.out,
        "service:   {}",
        if loaded { "running" } else { "not running" }
    );
    if let Some(state) = &state {
        let when = |key: &str| state[key].as_u64().map(iso8601);
        let text = |key: &str| state[key].as_str().map(str::to_owned);
        let _ = writeln!(io.out, "mode:      {}", text("mode").unwrap_or_default());
        let _ = writeln!(
            io.out,
            "started:   {}",
            when("started_at").unwrap_or_default()
        );
        let expiry = match state["expires_at"].as_u64() {
            Some(at) if at <= unix_now() => format!("{} (passed)", iso8601(at)),
            Some(at) => iso8601(at),
            None => "none".to_owned(),
        };
        let _ = writeln!(io.out, "expires:   {expiry}");
        let _ = writeln!(
            io.out,
            "session:   {}",
            text("session").unwrap_or_else(|| "(bridge default)".into())
        );
    }
    let audit_path = env.audit_path();
    let _ = writeln!(io.out, "audit log: {}", audit_path.display());
    if let Some(lines) = tail(&audit_path, 5).filter(|lines| !lines.is_empty()) {
        let _ = writeln!(io.out, "last {} audit lines:", lines.len());
        for line in lines {
            let _ = writeln!(io.out, "  {line}");
        }
    }
    Ok(0)
}

pub fn log(env: &TunnelEnv, count: usize, io: &mut Io) -> i32 {
    match tail(&env.audit_path(), count) {
        Some(lines) => {
            for line in lines {
                let _ = writeln!(io.out, "{line}");
            }
        }
        None => {
            let _ = writeln!(io.out, "no audit log yet: {}", env.audit_path().display());
        }
    }
    0
}
