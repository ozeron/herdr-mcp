//! The ngrok remote: ChatGPT reaches the bridge over an ngrok domain, behind a hosted OAuth login.
//!
//! `run` binds the HTTP server first, then starts `ngrok` as its child, so the port is ours.

use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use toml_edit::{Array, DocumentMut, value};

use super::doctor::{Level, Report};
use super::{
    JobState, MACOS_ONLY, NGROK, Runner, StartOptions, TunnelConfig, TunnelEnv, TunnelError,
    confirm_full, err_log_lines, expiry_text, job_state, last_error, launch, plist, private_dir,
    report, run_args, stop_service_for, tail, write_file, write_state,
};
use crate::audit::{iso8601, unix_now};
use crate::cli::ToolMode;
use crate::http::{self, TokenCheck};
use crate::install::Io;
use crate::oauth::{self, DENIED_PREFIX, JwtCheck};
use crate::serve::{BridgeOptions, HttpSpec, prepare_http};

const NGROK_BIN: &str = "ngrok";
const INSTALL_NGROK: &str = "brew install ngrok";
const ADD_AUTHTOKEN: &str = "ngrok config add-authtoken <token>";
const SERVING_MARK: &str = "herdr-mcp: serving ";
const SAVE_ISSUER_EXACTLY: &str =
    "save the issuer exactly as the provider's discovery document names it";
const STOP_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NgrokConfig {
    pub domain: String,
    pub issuer: String,
    pub audience: String,
    pub owners: Vec<String>,
}

impl NgrokConfig {
    pub fn url(&self) -> String {
        format!("https://{}", self.domain)
    }

    pub fn mcp_url(&self) -> String {
        format!("{}{}", self.url(), http::MCP_PATH)
    }

    fn to_toml(&self) -> String {
        let mut doc = DocumentMut::new();
        doc["domain"] = value(&self.domain);
        doc["issuer"] = value(&self.issuer);
        doc["audience"] = value(&self.audience);
        doc["owners"] = value(self.owners.iter().collect::<Array>());
        doc.to_string()
    }

    fn parse(text: &str) -> Result<Self, TunnelError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|err| TunnelError::new(format!("not valid TOML: {err}")))?;
        let text_of = |key: &str| {
            doc.get(key)
                .and_then(|item| item.as_str())
                .map(str::to_owned)
                .ok_or_else(|| TunnelError::new(format!("{key} is missing")))
        };
        let owners = doc
            .get("owners")
            .and_then(|item| item.as_array())
            .map(|owners| {
                owners
                    .iter()
                    .filter_map(|owner| owner.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Self::validated(
            &text_of("domain")?,
            &text_of("issuer")?,
            Some(&text_of("audience")?),
            owners,
        )
    }

    fn validated(
        domain: &str,
        issuer: &str,
        audience: Option<&str>,
        owners: Vec<String>,
    ) -> Result<Self, TunnelError> {
        let domain = validate_domain(domain)?;
        let issuer = validate_https("the issuer", issuer)?;
        let audience = match audience.map(str::trim).filter(|a| !a.is_empty()) {
            Some(audience) => validate_https("the audience", audience)?,
            None => format!("https://{domain}{}", http::MCP_PATH),
        };
        let owners: Vec<String> = owners
            .into_iter()
            .map(|owner| owner.trim().to_owned())
            .filter(|owner| !owner.is_empty())
            .collect();
        if owners.is_empty() {
            return Err(TunnelError::new(
                "name at least one owner: a token `sub` or a verified email",
            ));
        }
        Ok(Self {
            domain,
            issuer,
            audience,
            owners,
        })
    }
}

/// A bare hostname like `x.ngrok-free.dev`: no scheme, port or path.
fn validate_domain(raw: &str) -> Result<String, TunnelError> {
    let domain = raw.trim().to_ascii_lowercase();
    let label_ok = |label: &str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if domain.split('.').count() >= 2 && domain.split('.').all(label_ok) {
        Ok(domain)
    } else {
        Err(TunnelError::new(format!(
            "{raw:?} is not a bare domain like x.ngrok-free.dev: no scheme, port or path"
        )))
    }
}

fn validate_https(what: &'static str, raw: &str) -> Result<String, TunnelError> {
    oauth::validate_https(what, raw).map_err(|err| TunnelError::new(err.to_string()))
}

fn load_config(env: &TunnelEnv) -> Result<NgrokConfig, TunnelError> {
    let path = env.ngrok_config_path();
    let text = fs::read_to_string(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            TunnelError::usage("not set up: run `herdr-mcp tunnel ngrok setup`")
        } else {
            TunnelError::new(format!("{}: {err}", path.display()))
        }
    })?;
    NgrokConfig::parse(&text).map_err(|err| {
        TunnelError::new(format!(
            "{}: {err}; run `herdr-mcp tunnel ngrok setup`",
            path.display()
        ))
    })
}

enum NgrokCheck {
    Missing,
    Bad(PathBuf, String),
    Ok(PathBuf),
}

/// Finds `ngrok` and runs `ngrok config check`.
fn check_ngrok(env: &TunnelEnv) -> NgrokCheck {
    let Some(ngrok) = env.find_program(NGROK_BIN) else {
        return NgrokCheck::Missing;
    };
    let output = std::process::Command::new(&ngrok)
        .args(["config", "check"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => NgrokCheck::Ok(ngrok),
        Ok(out) => {
            let text = [out.stdout, out.stderr]
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .join("\n");
            let why = text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("it failed");
            NgrokCheck::Bad(ngrok, why.to_owned())
        }
        Err(err) => NgrokCheck::Bad(ngrok, err.to_string()),
    }
}

fn prompt(io: &mut Io, label: &str) -> Result<String, TunnelError> {
    let _ = write!(io.out, "{label}: ");
    let _ = io.out.flush();
    let mut line = String::new();
    io.input
        .read_line(&mut line)
        .map_err(|err| TunnelError::new(format!("cannot read the answer: {err}")))?;
    Ok(line.trim().to_owned())
}

pub async fn setup(env: &TunnelEnv, io: &mut Io<'_>, options: &DoctorOptions) -> i32 {
    let result = try_setup(env, io, options).await;
    report(io, result)
}

async fn try_setup(
    env: &TunnelEnv,
    io: &mut Io<'_>,
    options: &DoctorOptions,
) -> Result<i32, TunnelError> {
    if !io.stdin_is_tty {
        let _ = writeln!(
            io.err,
            "herdr-mcp: stdin is not a terminal; run `herdr-mcp tunnel ngrok setup` in one."
        );
        return Ok(2);
    }
    let domain = validate_domain(&prompt(io, "ngrok domain (like x.ngrok-free.dev)")?)?;
    let issuer = validate_https("the issuer", &prompt(io, "OAuth issuer (https URL)")?)?;
    let owners = prompt(io, "Owners, comma-separated (token sub or verified email)")?
        .split(',')
        .map(str::to_owned)
        .collect();
    let audience = prompt(io, &format!("Audience [https://{domain}/mcp]"))?;
    let config = NgrokConfig::validated(&domain, &issuer, Some(&audience), owners)?;

    let path = env.ngrok_config_path();
    write_file(&path, &config.to_toml())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|err| TunnelError::new(format!("{}: {err}", path.display())))?;
    let _ = writeln!(io.out, "Saved {}", path.display());

    // Best effort: an unreachable issuer is not a setup failure.
    if let Ok((body, _)) = discover(&config, options).await
        && let Some(named) = other_issuer(&config.issuer, &body)
    {
        let _ = writeln!(
            io.err,
            "warning: the issuer is saved as {}, but its discovery document names {named}; every login would fail. Fix: {SAVE_ISSUER_EXACTLY}.",
            config.issuer
        );
    }

    match check_ngrok(env) {
        NgrokCheck::Missing => {
            let _ = writeln!(
                io.err,
                "warning: ngrok not found. Install it: {INSTALL_NGROK}"
            );
        }
        NgrokCheck::Bad(_, why) => {
            let _ = writeln!(
                io.err,
                "warning: `ngrok config check` failed ({why}). Run: {ADD_AUTHTOKEN}"
            );
        }
        NgrokCheck::Ok(_) => {
            let _ = writeln!(io.out, "ngrok: config check passed");
        }
    }
    let _ = writeln!(
        io.out,
        "\nNext, in ChatGPT:
  1. Plugins -> + -> custom MCP server
  2. URL: {}
  3. Authentication: OAuth
At {}: allow dynamic client registration and PKCE, and the redirect https://chatgpt.com/connector/oauth/*.
Then run `herdr-mcp tunnel ngrok start` (read tools) or `herdr-mcp tunnel ngrok start --full`.",
        config.mcp_url(),
        config.issuer
    );
    Ok(0)
}

fn disclosure(env: &TunnelEnv, config: &NgrokConfig) -> String {
    format!(
        "FULL ACCESS

ChatGPT will be able to:
  - run shell commands as you
  - type into any pane
  - launch and close agents in the active Herdr session, and switch sessions

Pane output goes to OpenAI.
Anyone who signs in at {} as one of these owners gets this access: {}

To stop it:
  - run `herdr-mcp tunnel ngrok stop`
  - ask ChatGPT to call `herdr_tunnel_stop`
  - disable the plugin in ChatGPT
  - revoke sessions or rotate signing keys at the OAuth provider

Every call is logged to {}
",
        config.issuer,
        config.owners.join(", "),
        env.audit_path().display()
    )
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
    let config = load_config(env)?;
    if env.find_program(NGROK_BIN).is_none() {
        let _ = writeln!(
            io.err,
            "note: ngrok not found yet. Install it: {INSTALL_NGROK}"
        );
    }
    if options.full {
        confirm_full(
            &disclosure(env, &config),
            "herdr-mcp tunnel ngrok start",
            io,
        )?;
    }
    let expires_at = options.duration.map(|d| now + d.as_secs());
    launch(
        env,
        &NGROK,
        &plist(
            env,
            &NGROK,
            &["tunnel", "ngrok", "run"],
            &run_args(options, expires_at),
        ),
        runner,
    )?;
    let mode = write_state(env, &NGROK, options, now, expires_at)?;
    let expiry = expiry_text(expires_at);
    let _ = writeln!(
        io.out,
        "ngrok remote started in the background ({mode} tools, {expiry}).\nURL: {}\nStatus: herdr-mcp tunnel ngrok status\nStop:   herdr-mcp tunnel ngrok stop\nAudit log: {}",
        config.mcp_url(),
        env.audit_path().display()
    );
    Ok(0)
}

pub fn stop(env: &TunnelEnv, runner: &dyn Runner, io: &mut Io) -> i32 {
    if !cfg!(target_os = "macos") {
        let _ = writeln!(io.err, "herdr-mcp: {MACOS_ONLY}");
        return 1;
    }
    let result = stop_service_for(env, &NGROK, runner).map(|was_loaded| {
        let _ = writeln!(
            io.out,
            "{}",
            if was_loaded {
                "ngrok remote stopped."
            } else {
                "ngrok remote not running."
            }
        );
        0
    });
    report(io, result)
}

pub fn status(env: &TunnelEnv, runner: &dyn Runner, io: &mut Io) -> i32 {
    if !cfg!(target_os = "macos") {
        let _ = writeln!(io.err, "herdr-mcp: {MACOS_ONLY}");
        return 1;
    }
    let result = try_status(env, runner, io);
    report(io, result)
}

/// The reason of the last `auth denied:` line, if it came after the last start line.
fn last_denial(lines: &[String]) -> Option<&str> {
    let denied = lines
        .iter()
        .rposition(|line| line.contains(DENIED_PREFIX))?;
    let started = lines.iter().rposition(|line| line.contains(SERVING_MARK));
    if started.is_some_and(|started| started > denied) {
        return None;
    }
    lines[denied]
        .split_once(DENIED_PREFIX)
        .and_then(|(_, rest)| rest.split_whitespace().next())
}

fn try_status(env: &TunnelEnv, runner: &dyn Runner, io: &mut Io) -> Result<i32, TunnelError> {
    let job = job_state(env, &NGROK, runner)?;
    let state: Option<Value> = fs::read_to_string(env.state_file(&NGROK))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let service = match job {
        JobState::NotLoaded => "not running".to_owned(),
        JobState::Running | JobState::Loaded => "running".to_owned(),
        JobState::Starting => "starting".to_owned(),
        JobState::Failing(code) => format!(
            "failing (last exit {code}; launchd keeps restarting it): {}",
            last_error(env, &NGROK).unwrap_or_else(|| "no error output".to_owned())
        ),
    };
    let _ = writeln!(io.out, "service:   {service}");
    match load_config(env) {
        Ok(config) => {
            let _ = writeln!(io.out, "url:       {}", config.mcp_url());
            let _ = writeln!(io.out, "issuer:    {}", config.issuer);
            let _ = writeln!(io.out, "owners:    {}", config.owners.join(", "));
        }
        Err(err) => {
            let _ = writeln!(io.out, "settings:  {err}");
        }
    }
    if let Some(state) = &state {
        let text = |key: &str| state[key].as_str().map(str::to_owned);
        let _ = writeln!(io.out, "mode:      {}", text("mode").unwrap_or_default());
        let _ = writeln!(
            io.out,
            "started:   {}",
            state["started_at"]
                .as_u64()
                .map(iso8601)
                .unwrap_or_default()
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
    if let Some(reason) = last_denial(&err_log_lines(env, &NGROK)) {
        let _ = writeln!(io.out, "auth:      last denied: {reason}");
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

// ---- doctor ----

#[derive(Debug, Clone)]
pub struct DoctorOptions {
    pub http_timeout: Duration,
    /// Where to fetch the discovery document instead of the saved issuer. Tests only.
    #[doc(hidden)]
    pub discovery_base: Option<String>,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            http_timeout: Duration::from_secs(5),
            discovery_base: None,
        }
    }
}

/// `registration_endpoint` from an issuer discovery document, if non-empty.
pub fn registration_endpoint(discovery: &Value) -> Option<&str> {
    discovery["registration_endpoint"]
        .as_str()
        .filter(|endpoint| !endpoint.is_empty())
}

async fn discover(
    config: &NgrokConfig,
    options: &DoctorOptions,
) -> Result<(Value, String), String> {
    let client = oauth::http_client(options.http_timeout).map_err(|err| err.to_string())?;
    let base = options.discovery_base.as_deref().unwrap_or(&config.issuer);
    oauth::fetch_discovery(&client, base).await
}

/// The issuer a discovery document names, when it is not exactly `configured`. Tokens carry
/// the document's value and are compared byte for byte.
fn other_issuer<'a>(configured: &str, discovery: &'a Value) -> Option<&'a str> {
    let named = discovery["issuer"].as_str().unwrap_or("(none)");
    (named != configured).then_some(named)
}

pub async fn doctor(
    env: &TunnelEnv,
    runner: &dyn Runner,
    options: &DoctorOptions,
    io: &mut Io<'_>,
) -> i32 {
    let mut report = Report {
        io,
        fails: 0,
        warns: 0,
    };
    match check_ngrok(env) {
        NgrokCheck::Missing => report.line(Level::Fail, "ngrok: not found", Some(INSTALL_NGROK)),
        NgrokCheck::Bad(ngrok, why) => report.line(
            Level::Fail,
            format!("ngrok config check failed ({}): {why}", ngrok.display()),
            Some(ADD_AUTHTOKEN),
        ),
        NgrokCheck::Ok(ngrok) => report.line(
            Level::Pass,
            format!("ngrok: {}, config check passed", ngrok.display()),
            None,
        ),
    }

    match load_config(env) {
        Err(err) => {
            report.line(
                Level::Fail,
                format!("settings: {err}"),
                Some("run `herdr-mcp tunnel ngrok setup`"),
            );
            report.line(
                Level::Warn,
                "issuer discovery: skipped, it needs the settings",
                None,
            );
        }
        Ok(config) => {
            report.line(
                Level::Pass,
                format!(
                    "settings: {} -> {}, {} owner(s)",
                    config.mcp_url(),
                    config.issuer,
                    config.owners.len()
                ),
                None,
            );
            check_issuer(&config, options, &mut report).await;
        }
    }

    let job = job_state(env, &NGROK, runner).unwrap_or(JobState::NotLoaded);
    let start = Some("run `herdr-mcp tunnel ngrok start`");
    match &job {
        JobState::Running | JobState::Loaded => report.line(Level::Pass, "service: running", None),
        JobState::NotLoaded => report.line(Level::Warn, "service: not loaded", start),
        JobState::Starting => {
            report.line(Level::Warn, "service: starting, not running yet", start);
        }
        JobState::Failing(code) => report.line(
            Level::Warn,
            format!(
                "service: failing (last exit {code}): {}",
                last_error(env, &NGROK).unwrap_or_else(|| "no error output".to_owned())
            ),
            start,
        ),
    }

    let (fails, warns) = (report.fails, report.warns);
    let _ = if fails == 0 && warns == 0 {
        writeln!(report.io.out, "doctor: all checks passed")
    } else {
        writeln!(report.io.out, "doctor: {fails} failed, {warns} warnings")
    };
    i32::from(fails > 0)
}

async fn check_issuer(config: &NgrokConfig, options: &DoctorOptions, report: &mut Report<'_, '_>) {
    let (body, url) = match discover(config, options).await {
        Ok(found) => found,
        Err(why) => {
            report.line(
                Level::Fail,
                format!("issuer discovery: {why}"),
                Some("check the issuer URL in ngrok.toml; ChatGPT needs its discovery document"),
            );
            return;
        }
    };
    report.line(Level::Pass, format!("issuer discovery: {url}"), None);
    if let Some(named) = other_issuer(&config.issuer, &body) {
        report.line(
            Level::Fail,
            format!(
                "issuer discovery: it names issuer {named}, but ngrok.toml has {}",
                config.issuer
            ),
            Some(SAVE_ISSUER_EXACTLY),
        );
    }
    match registration_endpoint(&body) {
        Some(endpoint) => {
            report.line(
                Level::Pass,
                format!("registration_endpoint: {endpoint}"),
                None,
            );
        }
        None => report.line(
            Level::Warn,
            "registration_endpoint: missing; ChatGPT needs dynamic client registration",
            Some("enable dynamic client registration at the provider"),
        ),
    }
}

// ---- run ----

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub session: Option<String>,
    pub full: bool,
    /// Set only by `tunnel ngrok start --full`, after the typed disclosure.
    pub full_confirmed: bool,
    pub expires_at: Option<u64>,
}

/// Runs the bridge and `ngrok` in the foreground. Returns the exit code: 0 after a signal,
/// non-zero when ngrok or the server dies.
pub async fn run(env: &TunnelEnv, options: &RunOptions) -> i32 {
    match try_run(env, options).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            err.code()
        }
    }
}

async fn try_run(env: &TunnelEnv, options: &RunOptions) -> Result<i32, TunnelError> {
    if options.full && !options.full_confirmed {
        return Err(TunnelError::usage(
            "full access needs the typed disclosure: run `herdr-mcp tunnel ngrok start --full`",
        ));
    }
    let config = load_config(env)?;
    let ngrok = env
        .find_program(NGROK_BIN)
        .ok_or_else(|| TunnelError::new(format!("ngrok not found. Install it: {INSTALL_NGROK}")))?;
    let check: Arc<dyn TokenCheck> = Arc::new(
        JwtCheck::new(&config.issuer, &config.audience, config.owners.clone())
            .map_err(|err| TunnelError::usage(err.to_string()))?,
    );
    let stop = CancellationToken::new();
    let tunnel = TunnelConfig::for_service(NGROK, options.expires_at, Some(stop.clone()))?;
    let server = prepare_http(HttpSpec {
        bind: ([127, 0, 0, 1], 0).into(),
        public_url: Some(config.url()),
        check,
        metadata: Some((config.audience.clone(), config.issuer.clone())),
        bridge: BridgeOptions {
            session: options.session.clone(),
            client: None,
            max_wait_ms: None,
            tools: ToolMode::from_control(options.full),
            tunnel: Some(tunnel),
        },
    })
    .await
    .map_err(|err| TunnelError {
        message: err.message,
        code: i32::from(err.code),
    })?;
    let port = server
        .listener
        .local_addr()
        .map_err(|err| TunnelError::new(format!("no local address: {err}")))?
        .port();
    private_dir(&env.state_dir())?;

    let share = || {
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(Stdio::from)
            .map_err(|err| TunnelError::new(format!("cannot share stderr: {err}")))
    };
    let mut child = tokio::process::Command::new(&ngrok)
        .args(["http", &format!("127.0.0.1:{port}"), "--url", &config.url()])
        // The inspector at 127.0.0.1:4040 would keep request headers and bodies.
        .arg("--inspect=false")
        .args([
            "--log",
            "stdout",
            "--log-format",
            "json",
            "--log-level",
            "warn",
        ])
        .stdin(Stdio::null())
        .stdout(share()?)
        .stderr(share()?)
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| TunnelError::new(format!("cannot start ngrok: {err}")))?;

    let shutdown = server.shutdown.clone();
    tokio::spawn(http::drain_on_signal(
        shutdown.clone(),
        server.in_flight.clone(),
        server.draining.clone(),
    ));
    // `herdr_tunnel_stop` and the expiry reach a foreground run through this token.
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            stop.cancelled().await;
            shutdown.cancel();
        }
    });
    let mut serving = tokio::spawn(http::serve(
        server.listener,
        server.router,
        shutdown.clone(),
    ));
    eprintln!(
        "{SERVING_MARK}{} via ngrok (127.0.0.1:{port}, {} tools)",
        config.mcp_url(),
        if options.full { "control" } else { "read" }
    );
    Ok(supervise(&mut child, &mut serving, &shutdown).await)
}

/// Waits for the first of: ngrok exiting, the server stopping, or `shutdown`. Stops the others
/// and returns the exit code: 0 for a shutdown, 1 when ngrok or the server died.
#[doc(hidden)]
pub async fn supervise(
    child: &mut tokio::process::Child,
    serving: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    shutdown: &CancellationToken,
) -> i32 {
    tokio::select! {
        status = child.wait() => {
            let status = status.map_or_else(|err| err.to_string(), |s| s.to_string());
            eprintln!("herdr-mcp: ngrok exited ({status}); shutting down");
            shutdown.cancel();
            let _ = tokio::time::timeout(STOP_GRACE, serving).await;
            1
        }
        () = shutdown.cancelled() => {
            terminate(child).await;
            let _ = tokio::time::timeout(STOP_GRACE, serving).await;
            0
        }
        // The handle is spent once it resolves, so it is not awaited again.
        result = &mut *serving => {
            // A requested shutdown also ends the server, and can win the race with its own arm.
            let requested = shutdown.is_cancelled();
            if !requested {
                eprintln!("herdr-mcp: http server stopped: {result:?}");
            }
            terminate(child).await;
            shutdown.cancel();
            i32::from(!requested)
        }
    }
}

/// SIGTERM, then SIGKILL if ngrok has not gone after `STOP_GRACE`.
async fn terminate(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: signals our own child process.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}
