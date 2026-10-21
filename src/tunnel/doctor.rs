use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    Auth, CLIENT, JobState, KEY_ENV, OPENAI, Runner, TunnelEnv, auth_in_log, err_log_lines,
    has_status_code, is_auth_failure, is_auth_success, job_state, keychain_key, last_error,
    read_tunnel_id,
};
use crate::install::Io;

#[derive(Debug, Clone)]
pub struct DoctorOptions {
    /// How long the live probe waits for OpenAI to answer.
    pub probe_timeout: Duration,
    /// How long the bridge gets to answer `initialize` and `tools/list`.
    pub bridge_timeout: Duration,
    pub http_timeout: Duration,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            probe_timeout: Duration::from_secs(10),
            bridge_timeout: Duration::from_secs(5),
            http_timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Level {
    Pass,
    Warn,
    Fail,
}

pub(super) struct Report<'a, 'io> {
    pub(super) io: &'a mut Io<'io>,
    pub(super) fails: usize,
    pub(super) warns: usize,
}

impl Report<'_, '_> {
    pub(super) fn line(&mut self, level: Level, text: impl AsRef<str>, fix: Option<&str>) {
        let tag = match level {
            Level::Pass => "PASS",
            Level::Warn => {
                self.warns += 1;
                "WARN"
            }
            Level::Fail => {
                self.fails += 1;
                "FAIL"
            }
        };
        let _ = writeln!(self.io.out, "{tag}  {}", text.as_ref());
        if let Some(fix) = fix {
            let _ = writeln!(self.io.out, "      fix: {fix}");
        }
    }

    pub(super) fn note(&mut self, text: &str) {
        let _ = writeln!(self.io.out, "      note: {text}");
    }
}

pub fn doctor(env: &TunnelEnv, runner: &dyn Runner, options: &DoctorOptions, io: &mut Io) -> i32 {
    let mut report = Report {
        io,
        fails: 0,
        warns: 0,
    };

    let client = check_client(env, &mut report);
    let id = check_id(env, &mut report);
    let key = check_key(env, &mut report);
    check_bridge(env, options, &mut report);

    let job = job_state(env, &OPENAI, runner).unwrap_or(JobState::NotLoaded);
    match (&client, &id, &key) {
        (Some(client), Some(id), Some(key)) => {
            if job == JobState::NotLoaded {
                check_probe(client, id, key, options, &mut report);
            } else {
                check_log_auth(env, &mut report);
            }
        }
        _ => report.line(
            Level::Warn,
            "OpenAI key check: skipped, it needs tunnel-client, a tunnel ID and a key",
            None,
        ),
    }

    check_service(env, &job, &mut report);
    if matches!(job, JobState::Running | JobState::Loaded) {
        check_health(env, options, &mut report);
    }

    let (fails, warns) = (report.fails, report.warns);
    let _ = if fails == 0 && warns == 0 {
        writeln!(report.io.out, "doctor: all checks passed")
    } else {
        writeln!(report.io.out, "doctor: {fails} failed, {warns} warnings")
    };
    i32::from(fails > 0)
}

fn check_client(env: &TunnelEnv, report: &mut Report) -> Option<PathBuf> {
    let Some(client) = env.find_client() else {
        report.line(
            Level::Fail,
            format!("{CLIENT}: not found"),
            Some("brew install openai/tools/tunnel-client"),
        );
        return None;
    };
    let version = Command::new(&client)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(|line| line.trim().to_owned())
        })
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| "version unknown".to_owned());
    report.line(
        Level::Pass,
        format!("{CLIENT}: {version} ({})", client.display()),
        None,
    );
    Some(client)
}

fn check_id(env: &TunnelEnv, report: &mut Report) -> Option<String> {
    match read_tunnel_id(env) {
        Ok(id) => {
            report.line(Level::Pass, format!("tunnel ID: {id}"), None);
            Some(id)
        }
        Err(err) => {
            report.line(
                Level::Fail,
                format!("tunnel ID: {err}"),
                Some("run `herdr-mcp tunnel setup`"),
            );
            None
        }
    }
}

fn key_kind(key: &str) -> &'static str {
    ["sk-proj-", "sk-svcacct-", "sk-admin-", "sk-"]
        .into_iter()
        .find(|prefix| key.starts_with(prefix))
        .unwrap_or("unknown prefix")
}

fn check_key(env: &TunnelEnv, report: &mut Report) -> Option<String> {
    let (source, found) = match &env.api_key {
        Some(key) => (KEY_ENV, Ok(Some(key.clone()))),
        None => ("Keychain", keychain_key(env)),
    };
    let key = match found {
        Ok(Some(key)) => key,
        Ok(None) | Err(_) => {
            report.line(
                Level::Fail,
                format!("runtime key: none in {KEY_ENV} or the Keychain"),
                Some("run `herdr-mcp tunnel setup`"),
            );
            return None;
        }
    };
    let text = format!(
        "runtime key: {source}, {} key, {} characters",
        key_kind(&key),
        key.len()
    );
    if key.len() == 128 {
        report.line(
            Level::Warn,
            format!("{text}; looks truncated"),
            Some("rerun `herdr-mcp tunnel setup`"),
        );
    } else {
        report.line(Level::Pass, text, None);
    }
    Some(key)
}

/// A scratch directory the bridge probe uses for HOME and the XDG dirs, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir =
            std::env::temp_dir().join(format!("herdr-mcp-doctor-{}-{nanos}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Starts `bridge` in tunnel mode and returns its tool count once `tools/list` answers.
fn ask_bridge(bridge: &Path, timeout: Duration) -> Result<usize, String> {
    let scratch = Scratch::new().map_err(|err| format!("cannot make a temp dir: {err}"))?;
    let mut child = Command::new(bridge)
        .args(["--tools", "read", "--tunnel"])
        .env("HOME", &scratch.0)
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .env("XDG_STATE_HOME", scratch.0.join("state"))
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_SESSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot start it: {err}"))?;
    let result = converse(&mut child, timeout);
    kill(&mut child);
    result
}

fn converse(child: &mut Child, timeout: Duration) -> Result<usize, String> {
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let messages = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "herdr-mcp-doctor", "version": "0"},
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ];
    for message in messages {
        writeln!(stdin, "{message}").map_err(|err| format!("it closed its input: {err}"))?;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!("no answer within {}s", timeout.as_secs()));
            }
            Err(RecvTimeoutError::Disconnected) => return Err("it exited without answering".into()),
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value["id"] == 2 {
            return match value["result"]["tools"].as_array() {
                Some(tools) if !tools.is_empty() => Ok(tools.len()),
                Some(_) => Err("tools/list returned no tools".into()),
                None => Err(format!("tools/list failed: {}", value["error"]["message"])),
            };
        }
    }
}

fn check_bridge(env: &TunnelEnv, options: &DoctorOptions, report: &mut Report) {
    let bridge = env.bridge();
    if !bridge.exists() {
        report.line(
            Level::Fail,
            format!("bridge: {} does not exist", bridge.display()),
            Some("run `herdr plugin update herdr-mcp`"),
        );
        return;
    }
    match ask_bridge(&bridge, options.bridge_timeout) {
        Ok(count) => report.line(
            Level::Pass,
            format!(
                "bridge: {} answers MCP, {count} read tools",
                bridge.display()
            ),
            None,
        ),
        Err(why) => report.line(
            Level::Fail,
            format!("bridge: {}: {why}", bridge.display()),
            Some("run `herdr plugin update herdr-mcp`, then doctor again"),
        ),
    }
}

fn check_log_auth(env: &TunnelEnv, report: &mut Report) {
    match auth_in_log(&err_log_lines(env, &OPENAI)) {
        Auth::Rejected => report.line(
            Level::Fail,
            "OpenAI key: OpenAI rejected the key (401)",
            Some("run `herdr-mcp tunnel setup` with a whole, current key, then `herdr-mcp tunnel start`"),
        ),
        Auth::Accepted => report.line(
            Level::Pass,
            "OpenAI key: OpenAI accepted the key; tunnel connected",
            None,
        ),
        Auth::Unknown => report.line(
            Level::Warn,
            "OpenAI key: tunnel.err.log has no sign yet of OpenAI accepting or rejecting the key",
            Some("wait a few seconds and run doctor again"),
        ),
    }
}

enum Probe {
    Accepted,
    Rejected,
    Forbidden,
    Exited(Option<i32>, String),
    TimedOut,
}

fn redact(line: &str, key: &str) -> String {
    line.replace(key, "***")
}

fn run_probe(client: &Path, id: &str, key: &str, timeout: Duration) -> std::io::Result<Probe> {
    let mut child = Command::new(client)
        .args([
            "run",
            "--embedded-stateless-mcp-stub",
            "--control-plane.tunnel-id",
            id,
            "--health.listen-addr",
            "127.0.0.1:0",
        ])
        .env(KEY_ENV, key)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    let pipes: [Box<dyn Read + Send>; 2] = [
        Box::new(child.stdout.take().expect("piped")),
        Box::new(child.stderr.take().expect("piped")),
    ];
    for pipe in pipes {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    let verdict = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if is_auth_success(&line) => break Probe::Accepted,
            Ok(line) if is_auth_failure(&line) => break Probe::Rejected,
            Ok(line)
                if has_status_code(&line, "403") || line.to_lowercase().contains("permission") =>
            {
                break Probe::Forbidden;
            }
            Ok(line) => last = redact(&line, key),
            Err(RecvTimeoutError::Timeout) => break Probe::TimedOut,
            Err(RecvTimeoutError::Disconnected) => {
                break Probe::Exited(child.wait().ok().and_then(|s| s.code()), last);
            }
        }
    };
    kill(&mut child);
    Ok(verdict)
}

fn human(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn check_probe(client: &Path, id: &str, key: &str, options: &DoctorOptions, report: &mut Report) {
    report.note(
        "the probe briefly takes the tunnel; do not run doctor while another client serves this tunnel",
    );
    let setup = "run `herdr-mcp tunnel setup` with a whole, current key";
    match run_probe(client, id, key, options.probe_timeout) {
        Ok(Probe::Accepted) => report.line(
            Level::Pass,
            "OpenAI key: OpenAI accepted the key; tunnel metadata fetched",
            None,
        ),
        Ok(Probe::Rejected) => report.line(
            Level::Fail,
            "OpenAI key: OpenAI rejected the key (401 invalid_api_key)",
            Some(setup),
        ),
        Ok(Probe::Forbidden) => report.line(
            Level::Fail,
            "OpenAI key: OpenAI refused the key (403): the key lacks Tunnels Read+Use, or the tunnel is in another org or workspace",
            Some("create a key with Tunnels Read and Use in the tunnel's workspace, then run `herdr-mcp tunnel setup`"),
        ),
        Ok(Probe::Exited(code, last)) => report.line(
            Level::Fail,
            format!(
                "OpenAI key: tunnel-client exited (code {}) before connecting: {last}",
                code.map_or_else(|| "none".to_owned(), |c| c.to_string())
            ),
            Some("run `tunnel-client run --help` and check the install"),
        ),
        Ok(Probe::TimedOut) => report.line(
            Level::Warn,
            format!(
                "OpenAI key: no answer from OpenAI within {}",
                human(options.probe_timeout)
            ),
            Some("check the network, then run doctor again"),
        ),
        Err(err) => report.line(
            Level::Fail,
            format!("OpenAI key: cannot run {CLIENT}: {err}"),
            None,
        ),
    }
}

fn check_service(env: &TunnelEnv, job: &JobState, report: &mut Report) {
    let start = Some("run `herdr-mcp tunnel start`");
    match job {
        JobState::Running | JobState::Loaded => report.line(Level::Pass, "service: running", None),
        JobState::NotLoaded => report.line(Level::Warn, "service: not loaded", start),
        JobState::Starting => report.line(Level::Warn, "service: starting, not running yet", start),
        JobState::Failing(code) => report.line(
            Level::Warn,
            format!(
                "service: failing (last exit {code}): {}",
                last_error(env, &OPENAI).unwrap_or_else(|| "no error output".to_owned())
            ),
            start,
        ),
    }
}

/// The port of the newest `http://127.0.0.1:<port>` in the error log.
fn health_port(lines: &[String]) -> Option<u16> {
    const MARK: &str = "http://127.0.0.1:";
    lines.iter().rev().find_map(|line| {
        let at = line.rfind(MARK)? + MARK.len();
        let digits: String = line[at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok().filter(|port| *port != 0)
    })
}

fn http_get(port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|err| format!("cannot connect: {err}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("cannot send: {err}"))?;
    let mut raw = Vec::new();
    // A timeout after some bytes still leaves a readable response.
    let _ = stream.take(64 * 1024).read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or("no HTTP response")?;
    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    Ok((status, body.to_owned()))
}

fn check_health(env: &TunnelEnv, options: &DoctorOptions, report: &mut Report) {
    let Some(port) = health_port(&err_log_lines(env, &OPENAI)) else {
        report.line(
            Level::Warn,
            "tunnel health: no http://127.0.0.1:<port> in tunnel.err.log yet",
            Some("wait a few seconds and run doctor again"),
        );
        return;
    };
    match http_get(port, "/readyz", options.http_timeout) {
        Ok((200, _)) => report.line(
            Level::Pass,
            format!("tunnel ready: /readyz on port {port}"),
            None,
        ),
        Ok((status, _)) => report.line(
            Level::Fail,
            format!("tunnel ready: /readyz on port {port} returned {status}"),
            Some("run `herdr-mcp tunnel start` to restart it"),
        ),
        Err(why) => report.line(
            Level::Fail,
            format!("tunnel ready: /readyz on port {port}: {why}"),
            Some("run `herdr-mcp tunnel start` to restart it"),
        ),
    }
    match http_get(port, "/health/mcp", options.http_timeout) {
        Ok((200, body)) => {
            let (level, text) = describe_mcp_health(&body);
            report.line(level, format!("tunnel MCP: {text}"), None);
        }
        Ok((status, _)) => report.line(
            Level::Warn,
            format!("tunnel MCP: /health/mcp returned {status}"),
            None,
        ),
        Err(why) => report.line(Level::Warn, format!("tunnel MCP: /health/mcp: {why}"), None),
    }
}

/// One half of `/health/mcp`: `Some(true)` ok, `Some(false)` failed, `None` not observed yet.
fn observed_ok(part: &Value) -> Option<bool> {
    match part {
        Value::Object(map) => match map.get("ok") {
            Some(Value::Bool(ok)) => Some(*ok),
            _ => None,
        },
        _ => None,
    }
}

fn describe_mcp_health(body: &str) -> (Level, String) {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return (Level::Warn, "/health/mcp did not return JSON".to_owned());
    };
    // tunnel-client 0.0.16 nests both halves under `details` and reports `"ok": false` with a
    // top-level `"state": "not_observed"` until the first ChatGPT call.
    let details = &value["details"];
    let observed = value["state"].as_str() != Some("not_observed");
    let init = observed
        .then(|| observed_ok(&details["initialize"]))
        .flatten();
    let tools = observed
        .then(|| observed_ok(&details["tools_list"]))
        .flatten();
    let count = details["tools_list"]["tool_names"]
        .as_array()
        .map(|names| names.len() as u64)
        .filter(|_| tools == Some(true));
    let word = |ok: Option<bool>| match ok {
        Some(true) => "ok",
        Some(false) => "FAILED",
        None => "not observed",
    };
    let mut text = format!("initialize {}, tools/list {}", word(init), word(tools));
    if let Some(count) = count {
        text.push_str(&format!(", {count} tools"));
    }
    if init.is_none() && tools.is_none() {
        text.push_str(" (ChatGPT has not called yet)");
    }
    let level = if init == Some(false) || tools == Some(false) {
        Level::Warn
    } else {
        Level::Pass
    };
    (level, text)
}
