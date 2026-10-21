use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use herdr_mcp::install::Io;
use herdr_mcp::tunnel::ngrok::{self, DoctorOptions};
use herdr_mcp::tunnel::{Runner, StartOptions, TunnelEnv};
use serde_json::json;
use tempfile::TempDir;

const NOW: u64 = 1_800_000_000;
const DOMAIN: &str = "x.ngrok-free.dev";
const ISSUER: &str = "https://issuer.example.test";
const LABEL: &str = "dev.herdr-mcp.ngrok";

struct Sandbox {
    _tmp: TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["home", "bin", "xdg", "state", "sock"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { _tmp: tmp, root }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state/herdr-mcp")
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("xdg/herdr-mcp/ngrok.toml")
    }

    fn plist(&self) -> PathBuf {
        self.root
            .join(format!("home/Library/LaunchAgents/{LABEL}.plist"))
    }

    fn env(&self) -> TunnelEnv {
        TunnelEnv {
            home: self.root.join("home"),
            xdg_config_home: Some(self.root.join("xdg")),
            xdg_state_home: Some(self.root.join("state")),
            uid: 501,
            path: OsString::from(self.bin()),
            exe: PathBuf::from("/opt/herdr/bin/herdr-mcp"),
            api_key: None,
            fallback_dirs: Vec::new(),
        }
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.bin().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write_config(&self) {
        fs::create_dir_all(self.config_path().parent().unwrap()).unwrap();
        fs::write(
            self.config_path(),
            format!(
                "domain = \"{DOMAIN}\"\nissuer = \"{ISSUER}\"\naudience = \"https://{DOMAIN}/mcp\"\nowners = [\"alice@example.test\", \"user_1\"]\n"
            ),
        )
        .unwrap();
    }

    fn herdr_mcp(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mcp"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("HERDR_SOCKET_PATH", self.root.join("sock/herdr.sock"))
            .env("PATH", self.bin());
        command
    }
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn drive(tty: bool, input: &str, f: impl FnOnce(&mut Io) -> i32) -> Run {
    let mut reader = Cursor::new(input.as_bytes().to_vec());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = f(&mut Io {
        stdin_is_tty: tty,
        input: &mut reader,
        out: &mut out,
        err: &mut err,
    });
    Run {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

struct FakeRunner {
    loaded: bool,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeRunner {
    fn new(loaded: bool) -> Self {
        Self {
            loaded,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl Runner for FakeRunner {
    fn run(&self, _program: &Path, args: &[String]) -> std::io::Result<i32> {
        self.calls.lock().unwrap().push(args.to_vec());
        Ok(if args[0] == "print" && !self.loaded {
            1
        } else {
            0
        })
    }
}

fn plist_args(text: &str) -> Vec<String> {
    text.split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("</array>").next())
        .expect("ProgramArguments array")
        .split("<string>")
        .skip(1)
        .map(|part| part.split("</string>").next().unwrap().to_owned())
        .collect()
}

fn setup(sandbox: &Sandbox, tty: bool, input: &str) -> Run {
    let env = sandbox.env();
    drive(tty, input, |io| ngrok::setup(&env, io))
}

fn start(sandbox: &Sandbox, options: &StartOptions, tty: bool, input: &str) -> (Run, FakeRunner) {
    let env = sandbox.env();
    let runner = FakeRunner::new(false);
    let run = drive(tty, input, |io| {
        ngrok::start(&env, options, &runner, NOW, io)
    });
    (run, runner)
}

// ---- setup ----

#[test]
fn setup_stores_a_private_config() {
    let sandbox = Sandbox::new();
    let run = setup(
        &sandbox,
        true,
        &format!("{DOMAIN}\n{ISSUER}\nalice@example.test, user_1\n\n"),
    );
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    let mode = fs::metadata(sandbox.config_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let doc: toml_edit::DocumentMut = fs::read_to_string(sandbox.config_path())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(doc["domain"].as_str(), Some(DOMAIN));
    assert_eq!(doc["issuer"].as_str(), Some(ISSUER));
    let owners: Vec<&str> = doc["owners"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(owners, ["alice@example.test", "user_1"]);
    assert!(
        run.out.contains(&format!("https://{DOMAIN}/mcp")),
        "{}",
        run.out
    );
    assert!(run.out.contains("custom MCP server"), "{}", run.out);
}

#[test]
fn setup_audience_defaults_to_the_mcp_url_and_can_be_overridden() {
    let sandbox = Sandbox::new();
    setup(&sandbox, true, &format!("{DOMAIN}\n{ISSUER}\nu\n\n"));
    let text = fs::read_to_string(sandbox.config_path()).unwrap();
    assert!(
        text.contains(&format!("audience = \"https://{DOMAIN}/mcp\"")),
        "{text}"
    );

    let other = Sandbox::new();
    setup(
        &other,
        true,
        &format!("{DOMAIN}\n{ISSUER}\nu\nhttps://api.example.test\n"),
    );
    let text = fs::read_to_string(other.config_path()).unwrap();
    assert!(
        text.contains("audience = \"https://api.example.test\""),
        "{text}"
    );
}

#[test]
fn setup_rejects_bad_domains_and_an_http_issuer_and_writes_nothing() {
    for (domain, issuer) in [
        ("https://x.ngrok-free.dev", ISSUER),
        ("x.ngrok-free.dev/mcp", ISSUER),
        ("x.ngrok-free.dev:443", ISSUER),
        ("localhost", ISSUER),
        (DOMAIN, "http://issuer.example.test"),
        (DOMAIN, "issuer.example.test"),
    ] {
        let sandbox = Sandbox::new();
        let run = setup(&sandbox, true, &format!("{domain}\n{issuer}\nu\n\n"));
        assert_ne!(run.code, 0, "{domain} {issuer}");
        assert!(!sandbox.config_path().exists(), "{domain} {issuer}");
    }
}

#[test]
fn setup_needs_an_owner_and_a_terminal() {
    let sandbox = Sandbox::new();
    let run = setup(&sandbox, true, &format!("{DOMAIN}\n{ISSUER}\n , \n\n"));
    assert_ne!(run.code, 0);
    assert!(!sandbox.config_path().exists());
    let run = setup(&sandbox, false, "");
    assert_eq!(run.code, 2, "{}", run.err);
}

#[test]
fn setup_warns_when_ngrok_is_missing_and_passes_when_config_check_succeeds() {
    let sandbox = Sandbox::new();
    let input = format!("{DOMAIN}\n{ISSUER}\nu\n\n");
    let run = setup(&sandbox, true, &input);
    assert_eq!(run.code, 0);
    assert!(run.out.contains("ngrok not found") || run.err.contains("ngrok not found"));

    sandbox.script("ngrok", "exit 0");
    let run = setup(&sandbox, true, &input);
    assert!(!run.out.contains("ngrok not found") && !run.err.contains("ngrok not found"));
    sandbox.script("ngrok", "exit 1");
    let run = setup(&sandbox, true, &input);
    assert_eq!(run.code, 0);
    assert!(run.out.contains("config add-authtoken") || run.err.contains("config add-authtoken"));
}

// ---- start / stop / status ----

#[cfg(target_os = "macos")]
#[test]
fn start_without_config_points_at_setup_and_exits_2() {
    let sandbox = Sandbox::new();
    let (run, runner) = start(&sandbox, &StartOptions::default(), false, "");
    assert_eq!(run.code, 2);
    assert!(
        run.err.contains("herdr-mcp tunnel ngrok setup"),
        "{}",
        run.err
    );
    assert!(runner.calls().is_empty());
    assert!(!sandbox.plist().exists());
}

#[cfg(target_os = "macos")]
#[test]
fn start_writes_the_ngrok_plist_and_bootstraps_it() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    let (run, runner) = start(&sandbox, &StartOptions::default(), false, "");
    assert_eq!(run.code, 0, "{}", run.err);
    let text = fs::read_to_string(sandbox.plist()).unwrap();
    assert!(
        text.contains(&format!("<string>{LABEL}</string>")),
        "{text}"
    );
    assert_eq!(
        plist_args(&text),
        ["/opt/herdr/bin/herdr-mcp", "tunnel", "ngrok", "run"]
    );
    let state = sandbox.state_dir();
    assert!(
        text.contains(&format!("{}/ngrok.out.log", state.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("{}/ngrok.err.log", state.display())),
        "{text}"
    );
    let verbs: Vec<String> = runner.calls().iter().map(|c| c[0].clone()).collect();
    assert!(verbs.contains(&"bootstrap".to_owned()), "{verbs:?}");
    assert!(
        runner
            .calls()
            .iter()
            .all(|c| c.iter().all(|a| !a.contains("dev.herdr-mcp.tunnel"))),
        "{:?}",
        runner.calls()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn start_full_needs_the_typed_phrase_then_reaches_the_plist() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    let full = StartOptions {
        full: true,
        ..StartOptions::default()
    };
    let (run, runner) = start(&sandbox, &full, true, "yes\n");
    assert_ne!(run.code, 0);
    assert!(run.out.contains("FULL ACCESS"), "{}", run.out);
    assert!(!sandbox.plist().exists());
    assert!(runner.calls().is_empty());
    let (run, _) = start(&sandbox, &full, false, "");
    assert_eq!(run.code, 2, "{}", run.err);
    assert!(!sandbox.plist().exists());

    let (run, _) = start(&sandbox, &full, true, "full access\n");
    assert_eq!(run.code, 0, "{}", run.err);
    let args = plist_args(&fs::read_to_string(sandbox.plist()).unwrap());
    assert_eq!(args[1..], ["tunnel", "ngrok", "run", "--full"], "{args:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn start_for_writes_expires_at_and_session() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    let options = StartOptions {
        duration: Some(Duration::from_secs(7200)),
        session: Some("alpha".into()),
        ..StartOptions::default()
    };
    let (run, _) = start(&sandbox, &options, false, "");
    assert_eq!(run.code, 0, "{}", run.err);
    let args = plist_args(&fs::read_to_string(sandbox.plist()).unwrap());
    let at = (NOW + 7200).to_string();
    assert_eq!(
        args[1..],
        [
            "tunnel",
            "ngrok",
            "run",
            "--expires-at",
            at.as_str(),
            "--session",
            "alpha"
        ],
        "{args:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn stop_is_idempotent_and_removes_the_plist() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    start(&sandbox, &StartOptions::default(), false, "");
    assert!(sandbox.plist().exists());
    let env = sandbox.env();
    let runner = FakeRunner::new(true);
    let first = drive(false, "", |io| ngrok::stop(&env, &runner, io));
    assert_eq!(first.code, 0, "{}", first.err);
    assert!(!sandbox.plist().exists());
    assert!(
        runner
            .calls()
            .iter()
            .any(|c| c[0] == "bootout" && c[1].ends_with(LABEL)),
        "{:?}",
        runner.calls()
    );
    let idle = FakeRunner::new(false);
    let second = drive(false, "", |io| ngrok::stop(&env, &idle, io));
    assert_eq!(second.code, 0, "{}", second.err);
    assert!(idle.calls().iter().all(|c| c[0] == "print"));
}

#[cfg(target_os = "macos")]
#[test]
fn status_shows_mode_url_issuer_owners_and_expiry() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    let options = StartOptions {
        duration: Some(Duration::from_secs(3600)),
        ..StartOptions::default()
    };
    start(&sandbox, &options, false, "");
    let env = sandbox.env();
    let runner = FakeRunner::new(true);
    let run = drive(false, "", |io| ngrok::status(&env, &runner, io));
    assert_eq!(run.code, 0, "{}", run.err);
    for want in [
        "service:   running",
        "mode:      read",
        &format!("url:       https://{DOMAIN}/mcp"),
        ISSUER,
        "alice@example.test, user_1",
        "expires:   2027",
        "tunnel-audit.log",
    ] {
        assert!(run.out.contains(want), "missing {want:?} in\n{}", run.out);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn status_shows_the_last_auth_denial_only_when_newer_than_the_last_start() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    fs::create_dir_all(sandbox.state_dir()).unwrap();
    let log = sandbox.state_dir().join("ngrok.err.log");
    let status = || {
        let env = sandbox.env();
        let runner = FakeRunner::new(true);
        drive(false, "", |io| ngrok::status(&env, &runner, io)).out
    };
    fs::write(
        &log,
        "herdr-mcp: serving https://x/mcp via ngrok\nherdr-mcp: auth denied: not_owner\n",
    )
    .unwrap();
    assert!(status().contains("not_owner"));
    fs::write(
        &log,
        "herdr-mcp: auth denied: not_owner\nherdr-mcp: serving https://x/mcp via ngrok\n",
    )
    .unwrap();
    assert!(!status().contains("not_owner"));
}

// ---- doctor ----

fn run_doctor(sandbox: &Sandbox) -> Run {
    let env = sandbox.env();
    let runner = FakeRunner::new(false);
    let mut reader = Cursor::new(Vec::new());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(ngrok::doctor(
            &env,
            &runner,
            &DoctorOptions::default(),
            &mut Io {
                stdin_is_tty: false,
                input: &mut reader,
                out: &mut out,
                err: &mut err,
            },
        ));
    Run {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

#[test]
fn doctor_reports_a_missing_ngrok_and_a_missing_config() {
    let sandbox = Sandbox::new();
    let run = run_doctor(&sandbox);
    assert_ne!(run.code, 0, "{}", run.out);
    assert!(run.out.contains("FAIL  ngrok: not found"), "{}", run.out);
    assert!(run.out.contains("FAIL  settings:"), "{}", run.out);
    assert!(
        run.out.contains("herdr-mcp tunnel ngrok setup"),
        "{}",
        run.out
    );
}

#[test]
fn doctor_flags_a_failing_ngrok_config_check() {
    let sandbox = Sandbox::new();
    sandbox.script("ngrok", "echo bad authtoken; exit 1");
    let run = run_doctor(&sandbox);
    assert!(run.out.contains("FAIL  ngrok config check"), "{}", run.out);
    assert!(run.out.contains("config add-authtoken"), "{}", run.out);
}

#[test]
fn registration_endpoint_is_read_from_the_discovery_body() {
    let with =
        json!({"issuer": ISSUER, "registration_endpoint": "https://issuer.example.test/register"});
    assert_eq!(
        ngrok::registration_endpoint(&with),
        Some("https://issuer.example.test/register")
    );
    let without = json!({"issuer": ISSUER});
    assert_eq!(ngrok::registration_endpoint(&without), None);
    assert_eq!(
        ngrok::registration_endpoint(&json!({"registration_endpoint": ""})),
        None
    );
}

#[tokio::test]
async fn discovery_falls_back_to_the_authorization_server_document() {
    use axum::Router;
    use axum::routing::get;
    let app = Router::new().route(
        "/.well-known/oauth-authorization-server",
        get(|| async {
            (
                [("content-type", "application/json")],
                json!({"registration_endpoint": "https://i/register"}).to_string(),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let (body, found) = ngrok::fetch_discovery(&url, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        found.ends_with("/.well-known/oauth-authorization-server"),
        "{found}"
    );
    assert_eq!(
        ngrok::registration_endpoint(&body),
        Some("https://i/register")
    );
    let dead = ngrok::fetch_discovery("http://127.0.0.1:1", Duration::from_secs(2)).await;
    assert!(dead.is_err());
}

// ---- run (the real binary) ----

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

const FAKE_NGROK: &str = r#"rec="$REC"
echo "argv:$*" > "$rec"
echo "pid:$$" >> "$rec"
port="${2##*:}"
if /bin/bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" 2>/dev/null; then echo connect:ok >> "$rec"; else echo connect:fail >> "$rec"; fi
"#;

fn fake_ngrok(sandbox: &Sandbox, tail: &str) -> PathBuf {
    let rec = sandbox.root.join("ngrok.rec");
    sandbox.script(
        "ngrok",
        &format!("REC='{}'\n{FAKE_NGROK}{tail}", rec.display()),
    );
    rec
}

fn spawn_run(sandbox: &Sandbox, extra: &[&str]) -> Guard {
    let mut args = vec!["tunnel", "ngrok", "run"];
    args.extend(extra);
    Guard(
        sandbox
            .herdr_mcp(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

fn recorded(rec: &Path) -> String {
    fs::read_to_string(rec).unwrap_or_default()
}

#[test]
fn run_binds_before_ngrok_starts_and_refuses_tokenless_requests() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    let rec = fake_ngrok(&sandbox, "exec /bin/sleep 60");
    let mut run = spawn_run(&sandbox, &[]);
    wait_until("ngrok to record a connect", || {
        recorded(&rec).contains("connect:")
    });
    let text = recorded(&rec);
    assert!(
        text.contains("connect:ok"),
        "the port was not open yet: {text}"
    );
    let argv = text.lines().next().unwrap();
    let port: u16 = argv
        .split_whitespace()
        .nth(1)
        .and_then(|a| a.strip_prefix("127.0.0.1:"))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("bad argv {argv}"));
    assert_eq!(
        argv,
        format!("argv:http 127.0.0.1:{port} --url https://{DOMAIN} --log stdout --log-format json")
    );

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    assert!(reply.to_lowercase().contains("www-authenticate"), "{reply}");

    let pid: i32 = text
        .lines()
        .find_map(|l| l.strip_prefix("pid:"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signalling our own child process.
    unsafe { libc::kill(run.0.id() as i32, libc::SIGTERM) };
    wait_until("run to exit", || run.0.try_wait().unwrap().is_some());
    assert!(run.0.try_wait().unwrap().unwrap().success());
    // SAFETY: signal 0 only probes whether the pid exists.
    wait_until("ngrok to be killed", || unsafe { libc::kill(pid, 0) } != 0);
}

#[test]
fn run_exits_non_zero_when_ngrok_exits() {
    let sandbox = Sandbox::new();
    sandbox.write_config();
    let rec = fake_ngrok(&sandbox, "exit 3");
    let mut run = spawn_run(&sandbox, &[]);
    wait_until("run to exit", || run.0.try_wait().unwrap().is_some());
    assert!(!run.0.try_wait().unwrap().unwrap().success());
    assert!(recorded(&rec).contains("connect:ok"));
}

#[test]
fn run_without_config_exits_2() {
    let sandbox = Sandbox::new();
    fake_ngrok(&sandbox, "exit 0");
    let out = sandbox
        .herdr_mcp(&["tunnel", "ngrok", "run"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("tunnel ngrok setup"));
}
