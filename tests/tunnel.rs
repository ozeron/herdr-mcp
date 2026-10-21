use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

use herdr_mcp::install::Io;
use herdr_mcp::tunnel::{
    RunOptions, Runner, StartOptions, TunnelEnv, log, parse_duration, prepare_run, setup, start,
    status, stop, validate_tunnel_id,
};
use tempfile::TempDir;

const KEY: &str = "sk-test-runtime-key-123";
const ID: &str = "tunnel_0123456789abcdef0123456789abcdef";

struct Sandbox {
    _tmp: TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["home", "bin", "xdg", "state"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { _tmp: tmp, root }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn record(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.rec"))
    }

    fn recorded(&self, name: &str) -> String {
        fs::read_to_string(self.record(name)).unwrap_or_default()
    }

    /// A fake CLI that records its argv and whether the key is in its env, prints `stdout`
    /// and exits with `code`.
    fn fake(&self, name: &str, stdout: &str, code: i32) {
        let rec = self.record(name);
        let script = format!(
            "#!/bin/sh\n{{ printf 'argv:%s\\n' \"$*\"; if [ -n \"${{CONTROL_PLANE_API_KEY+x}}\" ]; then echo key:set; else echo key:unset; fi; }} >> '{}'\nprintf '%s' '{stdout}'\nexit {code}\n",
            rec.display()
        );
        let path = self.bin().join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn save_id(&self, id: &str) {
        let path = self.root.join("xdg/herdr-mcp/tunnel");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{id}\n")).unwrap();
    }

    fn env(&self) -> TunnelEnv {
        TunnelEnv {
            home: self.home(),
            xdg_config_home: Some(self.root.join("xdg")),
            xdg_state_home: Some(self.root.join("state")),
            uid: 501,
            path: OsString::from(self.bin()),
            exe: PathBuf::from("/opt/herdr/bin/herdr-mcp"),
            api_key: None,
            fallback_dirs: Vec::new(),
        }
    }

    fn herdr_mcp(&self, args: &[&str], key: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mcp"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("PATH", self.bin());
        if let Some(key) = key {
            command.env("CONTROL_PLANE_API_KEY", key);
        }
        command.output().unwrap()
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

fn args_of(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn prepared(env: &TunnelEnv, session: Option<&str>) -> Command {
    let options = RunOptions {
        session: session.map(str::to_owned),
        ..RunOptions::default()
    };
    prepared_with(env, &options)
}

fn prepared_with(env: &TunnelEnv, options: &RunOptions) -> Command {
    let mut command = None;
    drive(false, "", |io| {
        command = prepare_run(env, options, io).ok();
        0
    });
    command.expect("prepare_run succeeds")
}

#[test]
fn validator_accepts_prefixed_ids_and_rejects_the_rest() {
    assert_eq!(validate_tunnel_id(&format!("{ID}\n")).unwrap(), ID);
    for bad in [
        "",
        "tunnel_",
        "abc",
        "tunnel_abc",
        "Tunnel_0123456789abcdef0123456789abcdef",
        "tunnel_0123456789ABCDEF0123456789abcdef",
        "tunnel_0123456789abcdef0123456789abcde",
        "tunnel_0123456789abcdef0123456789abcdef0",
        "tunnel_0123456789abcdef 123456789abcdef",
        "tunnel-0123456789abcdef0123456789abcdef",
        " ",
    ] {
        assert!(validate_tunnel_id(bad).is_err(), "{bad:?} must be rejected");
    }
}

#[test]
fn command_has_the_documented_argv_and_the_key_only_in_the_child_env() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 0);
    sandbox.fake("security", &format!("{KEY}\n"), 0);
    sandbox.save_id(ID);
    let env = sandbox.env();
    let command = prepared(&env, None);
    assert_eq!(command.get_program(), sandbox.bin().join("tunnel-client"));
    assert_eq!(
        args_of(&command),
        [
            "run",
            "--control-plane.tunnel-id",
            ID,
            "--mcp-command",
            &format!("{} --tools read --tunnel", env.exe.display()),
            "--health.listen-addr",
            "127.0.0.1:0",
        ]
    );
    let key = command
        .get_envs()
        .find(|(name, _)| *name == "CONTROL_PLANE_API_KEY")
        .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
    assert_eq!(key.as_deref(), Some(KEY));
    assert!(!args_of(&command).iter().any(|arg| arg.contains(KEY)));
    assert!(std::env::var_os("CONTROL_PLANE_API_KEY").is_none());
}

#[test]
fn bridge_is_the_shim_when_it_exists_and_quoted_when_it_has_spaces() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 0);
    sandbox.fake("security", KEY, 0);
    sandbox.save_id(ID);
    let mut env = sandbox.env();
    env.home = sandbox.root.join("my home");
    let shim = env.home.join(".local/bin/herdr-mcp");
    fs::create_dir_all(shim.parent().unwrap()).unwrap();
    fs::write(&shim, "").unwrap();
    let command = prepared(&env, None);
    assert_eq!(
        args_of(&command)[4],
        format!("\"{}\" --tools read --tunnel", shim.display())
    );
}

#[test]
fn session_reaches_the_bridge_command() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 0);
    sandbox.fake("security", KEY, 0);
    sandbox.save_id(ID);
    let env = sandbox.env();
    let command = prepared(&env, Some("foo"));
    assert_eq!(
        args_of(&command)[4],
        format!("{} --tools read --tunnel --session foo", env.exe.display())
    );
}

#[test]
fn missing_tunnel_client_names_the_brew_command() {
    let sandbox = Sandbox::new();
    sandbox.save_id(ID);
    let env = sandbox.env();
    let mut err = None;
    drive(false, "", |io| {
        err = prepare_run(&env, &RunOptions::default(), io)
            .err()
            .map(|e| e.to_string());
        0
    });
    let err = err.expect("fails");
    assert!(
        err.contains("brew install openai/tools/tunnel-client"),
        "{err}"
    );
}

#[test]
fn missing_tunnel_id_points_at_setup() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 0);
    let env = sandbox.env();
    let mut err = None;
    drive(false, "", |io| {
        err = prepare_run(&env, &RunOptions::default(), io)
            .err()
            .map(|e| e.to_string());
        0
    });
    assert!(err.unwrap().contains("herdr-mcp tunnel setup"));
}

#[test]
fn end_to_end_run_execs_tunnel_client_with_its_exit_code_and_never_prints_the_key() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 7);
    sandbox.fake("security", &format!("{KEY}\n"), 0);
    sandbox.save_id(ID);
    let output = sandbox.herdr_mcp(&["tunnel", "run", "--session", "foo"], None);
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!all.contains(KEY), "{all}");
    assert!(all.contains("Read tools only"), "{all}");
    let rec = sandbox.recorded("tunnel-client");
    assert!(rec.contains("key:set"), "{rec}");
    assert!(
        rec.contains(&format!("--control-plane.tunnel-id {ID}")),
        "{rec}"
    );
    assert!(rec.contains("--tools read --tunnel --session foo"), "{rec}");
    assert!(rec.contains("--health.listen-addr 127.0.0.1:0"), "{rec}");
    assert!(!rec.contains(KEY), "{rec}");
    let security = sandbox.recorded("security");
    assert!(
        security.contains("find-generic-password -s herdr-mcp-tunnel -a runtime-key -w"),
        "{security}"
    );
}

#[test]
fn preset_key_skips_the_keychain() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 0);
    sandbox.fake("security", "from-keychain", 0);
    sandbox.save_id(ID);
    let output = sandbox.herdr_mcp(&["tunnel", "run"], Some(KEY));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(sandbox.recorded("security"), "", "security must not run");
    assert!(sandbox.recorded("tunnel-client").contains("key:set"));
}

#[test]
fn end_to_end_failures_exit_non_zero_with_a_pointer() {
    let sandbox = Sandbox::new();
    let output = sandbox.herdr_mcp(&["tunnel", "run"], Some(KEY));
    // No tunnel-client on the sandbox PATH; only a machine with one in a fallback dir passes.
    if Path::new("/opt/homebrew/bin/tunnel-client").exists()
        || Path::new("/usr/local/bin/tunnel-client").exists()
    {
        return;
    }
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("brew install openai/tools/tunnel-client"),
        "{err}"
    );
    assert!(!err.contains(KEY));
}

#[test]
fn setup_without_a_tty_errors_and_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.fake("security", "", 0);
    let output = sandbox.herdr_mcp(&["tunnel", "setup"], None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a terminal"));
    assert_eq!(sandbox.recorded("security"), "");
    assert!(!sandbox.root.join("xdg/herdr-mcp/tunnel").exists());
}

#[test]
fn setup_stores_the_key_via_security_and_saves_the_id_privately() {
    let sandbox = Sandbox::new();
    sandbox.fake("security", "", 0);
    let env = sandbox.env();
    let run = drive(true, &format!("{ID}\n"), |io| setup(&env, io));
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert!(
        sandbox
            .recorded("security")
            .contains("argv:add-generic-password -U -s herdr-mcp-tunnel -a runtime-key -w\n"),
        "-w must be last so security prompts for the secret"
    );
    let path = sandbox.root.join("xdg/herdr-mcp/tunnel");
    assert_eq!(fs::read_to_string(&path).unwrap().trim(), ID);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for needle in [
        "Plugins",
        "Add custom MCP server",
        "Tunnel",
        "risk warning",
        "Create as a plugin",
        "admin",
        "who in the workspace",
    ] {
        assert!(run.out.contains(needle), "missing {needle}: {}", run.out);
    }
}

#[test]
fn setup_rejects_a_bad_id_before_touching_the_keychain() {
    let sandbox = Sandbox::new();
    sandbox.fake("security", "", 0);
    let env = sandbox.env();
    let run = drive(true, "nope\n", |io| setup(&env, io));
    assert_eq!(run.code, 1);
    assert!(run.err.contains("tunnel_"), "{}", run.err);
    assert_eq!(sandbox.recorded("security"), "");
}

// ---- U3: background service, full mode, status and log ----

const LABEL: &str = "dev.herdr-mcp.tunnel";
const NOW: u64 = 1_800_000_000;

/// Records every launchctl call; `print` reports the agent as loaded when `loaded` is set.
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

    fn verbs(&self) -> Vec<String> {
        self.calls().iter().map(|c| c[0].clone()).collect()
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

impl Sandbox {
    fn ready(&self) {
        self.fake("tunnel-client", "", 0);
        self.fake("security", &format!("{KEY}\n"), 0);
        self.save_id(ID);
    }

    fn plist(&self) -> PathBuf {
        self.home()
            .join(format!("Library/LaunchAgents/{LABEL}.plist"))
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state/herdr-mcp")
    }

    fn audit(&self) -> PathBuf {
        self.state_dir().join("tunnel-audit.log")
    }

    fn write_audit(&self, count: usize) {
        fs::create_dir_all(self.state_dir()).unwrap();
        let text: String = (1..=count)
            .map(|n| format!("{{\"tool\":\"herdr_run\",\"n\":{n}}}\n"))
            .collect();
        fs::write(self.audit(), text).unwrap();
    }

    fn write_state(&self, mode: &str, expires_at: Option<u64>) {
        fs::create_dir_all(self.state_dir()).unwrap();
        let state = serde_json::json!({
            "mode": mode, "started_at": NOW, "expires_at": expires_at, "session": "foo",
        });
        fs::write(self.state_dir().join("tunnel.state"), state.to_string()).unwrap();
    }

    fn fake_launchctl(&self, loaded: bool) {
        let rec = self.record("launchctl");
        let print_code = if loaded { 0 } else { 1 };
        let script = format!(
            "#!/bin/sh\necho \"argv:$*\" >> '{}'\n[ \"$1\" = print ] && exit {print_code}\nexit 0\n",
            rec.display()
        );
        let path = self.bin().join("launchctl");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn start_with(
    sandbox: &Sandbox,
    runner: &FakeRunner,
    options: &StartOptions,
    tty: bool,
    input: &str,
) -> Run {
    let env = sandbox.env();
    drive(tty, input, |io| start(&env, options, runner, NOW, io))
}

fn plist_args(text: &str) -> Vec<String> {
    let section = text
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("</array>").next())
        .expect("ProgramArguments array");
    section
        .split("<string>")
        .skip(1)
        .map(|part| part.split("</string>").next().unwrap().to_owned())
        .collect()
}

#[test]
fn durations_parse_with_unit_suffixes() {
    use std::time::Duration;
    assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
    assert_eq!(parse_duration("8h").unwrap(), Duration::from_secs(8 * 3600));
    assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
    assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
    for bad in ["", "m", "30", "0m", "-5m", "1w", "abc", "1.5h", "5 m"] {
        assert!(parse_duration(bad).is_err(), "{bad:?}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn start_writes_the_read_plist_then_bootstraps() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let runner = FakeRunner::new(false);
    let options = StartOptions {
        full: false,
        duration: Some(std::time::Duration::from_secs(8 * 3600)),
        session: Some("foo".into()),
    };
    let run = start_with(&sandbox, &runner, &options, false, "");
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);

    let text = fs::read_to_string(sandbox.plist()).unwrap();
    assert_eq!(
        plist_args(&text),
        [
            "/opt/herdr/bin/herdr-mcp",
            "tunnel",
            "run",
            "--expires-at",
            &(NOW + 8 * 3600).to_string(),
            "--session",
            "foo",
        ]
    );
    for needle in [
        &format!("<key>Label</key>\n  <string>{LABEL}</string>"),
        "<key>RunAtLoad</key>\n  <true/>",
        "<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>",
        &format!(
            "<key>StandardOutPath</key>\n  <string>{}</string>",
            sandbox.state_dir().join("tunnel.out.log").display()
        ),
        &format!(
            "<key>StandardErrorPath</key>\n  <string>{}</string>",
            sandbox.state_dir().join("tunnel.err.log").display()
        ),
        &format!(
            "<key>HOME</key>\n    <string>{}</string>",
            sandbox.home().display()
        ),
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
    assert!(!text.contains(KEY), "the key never reaches the plist");
    assert!(!text.contains("--full"), "{text}");
    assert!(!text.contains("control"), "{text}");

    assert_eq!(runner.verbs(), ["print", "bootstrap"]);
    assert_eq!(
        runner.calls()[1],
        [
            "bootstrap".to_string(),
            "gui/501".to_string(),
            sandbox.plist().display().to_string()
        ]
    );
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(sandbox.state_dir().join("tunnel.state")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["mode"], "read");
    assert_eq!(state["started_at"], NOW);
    assert_eq!(state["expires_at"], NOW + 8 * 3600);
    assert_eq!(state["session"], "foo");
}

#[cfg(target_os = "macos")]
#[test]
fn start_boots_out_an_already_loaded_agent_first() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let runner = FakeRunner::new(true);
    let run = start_with(&sandbox, &runner, &StartOptions::default(), false, "");
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert_eq!(runner.verbs(), ["print", "bootout", "bootstrap"]);
    assert_eq!(runner.calls()[1][1], format!("gui/501/{LABEL}"));
    let text = fs::read_to_string(sandbox.plist()).unwrap();
    assert_eq!(
        plist_args(&text),
        ["/opt/herdr/bin/herdr-mcp", "tunnel", "run"]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn start_full_needs_the_exact_typed_phrase() {
    for (tty, input) in [
        (false, ""),
        (false, "full access\n"),
        (true, "yes\n"),
        (true, "Full Access\n"),
        (true, "full access please\n"),
        (true, ""),
    ] {
        let sandbox = Sandbox::new();
        sandbox.ready();
        let runner = FakeRunner::new(false);
        let options = StartOptions {
            full: true,
            ..StartOptions::default()
        };
        let run = start_with(&sandbox, &runner, &options, tty, input);
        assert_ne!(run.code, 0, "tty={tty} input={input:?}");
        assert!(!sandbox.plist().exists(), "tty={tty} input={input:?}");
        assert!(!sandbox.state_dir().join("tunnel.state").exists());
        assert!(runner.calls().is_empty(), "launchctl must not run");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn start_full_shows_the_disclosure_then_writes_the_confirmed_plist() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let runner = FakeRunner::new(false);
    let options = StartOptions {
        full: true,
        ..StartOptions::default()
    };
    let run = start_with(&sandbox, &runner, &options, true, "full access\n");
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    for needle in [
        "run shell commands as you",
        "type into any pane",
        "launch and close agents",
        "switch sessions",
        "OpenAI",
        "Anyone who can use your ChatGPT connector",
        "herdr-mcp tunnel stop",
        "herdr_tunnel_stop",
        "disable the plugin in ChatGPT",
        "revoke the runtime key",
        "full access",
        sandbox.audit().to_str().unwrap(),
    ] {
        assert!(run.out.contains(needle), "missing {needle:?}:\n{}", run.out);
    }
    let text = fs::read_to_string(sandbox.plist()).unwrap();
    assert_eq!(
        plist_args(&text),
        [
            "/opt/herdr/bin/herdr-mcp",
            "tunnel",
            "run",
            "--full",
            "--full-confirmed"
        ]
    );
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(sandbox.state_dir().join("tunnel.state")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["mode"], "full");
    assert!(state["expires_at"].is_null());
}

#[cfg(target_os = "macos")]
#[test]
fn start_without_setup_points_at_setup_and_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.fake("tunnel-client", "", 0);
    sandbox.fake("security", "", 44);
    let runner = FakeRunner::new(false);
    let run = start_with(&sandbox, &runner, &StartOptions::default(), false, "");
    assert_ne!(run.code, 0);
    assert!(run.err.contains("herdr-mcp tunnel setup"), "{}", run.err);
    assert!(!sandbox.plist().exists());
    assert!(runner.calls().is_empty());

    sandbox.save_id(ID);
    let run = start_with(&sandbox, &runner, &StartOptions::default(), false, "");
    assert_ne!(run.code, 0);
    assert!(run.err.contains("herdr-mcp tunnel setup"), "{}", run.err);
    assert!(!sandbox.plist().exists());
}

#[cfg(target_os = "macos")]
#[test]
fn start_without_tunnel_client_names_the_brew_command() {
    let sandbox = Sandbox::new();
    sandbox.fake("security", KEY, 0);
    sandbox.save_id(ID);
    let runner = FakeRunner::new(false);
    let run = start_with(&sandbox, &runner, &StartOptions::default(), false, "");
    assert_ne!(run.code, 0);
    assert!(
        run.err.contains("brew install openai/tools/tunnel-client"),
        "{}",
        run.err
    );
    assert!(!sandbox.plist().exists());
}

#[cfg(target_os = "macos")]
#[test]
fn stop_boots_the_agent_out_and_removes_the_plist_and_state() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let runner = FakeRunner::new(true);
    start_with(&sandbox, &runner, &StartOptions::default(), false, "");
    assert!(sandbox.plist().exists());

    let runner = FakeRunner::new(true);
    let env = sandbox.env();
    let run = drive(false, "", |io| stop(&env, &runner, io));
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert_eq!(runner.verbs(), ["print", "bootout"]);
    assert_eq!(runner.calls()[1], ["bootout", &format!("gui/501/{LABEL}")]);
    assert!(!sandbox.plist().exists());
    assert!(!sandbox.state_dir().join("tunnel.state").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn stop_when_nothing_runs_is_fine() {
    let sandbox = Sandbox::new();
    let runner = FakeRunner::new(false);
    let env = sandbox.env();
    let run = drive(false, "", |io| stop(&env, &runner, io));
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert!(run.out.contains("not running"), "{}", run.out);
    assert!(!runner.verbs().contains(&"bootout".to_string()));
}

#[cfg(target_os = "macos")]
#[test]
fn status_reports_running_mode_times_session_audit_path_and_the_last_five_lines() {
    let sandbox = Sandbox::new();
    sandbox.write_state("full", Some(NOW + 3600));
    sandbox.write_audit(7);
    let runner = FakeRunner::new(true);
    let env = sandbox.env();
    let run = drive(false, "", |io| status(&env, &runner, io));
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert_eq!(runner.calls()[0], ["print", &format!("gui/501/{LABEL}")]);
    for needle in [
        "running",
        "full",
        "2027-01-15T08:00:00Z",
        "2027-01-15T09:00:00Z",
        "foo",
        sandbox.audit().to_str().unwrap(),
        "\"n\":3",
        "\"n\":7",
    ] {
        assert!(run.out.contains(needle), "missing {needle:?}:\n{}", run.out);
    }
    assert!(
        !run.out.contains("\"n\":2}"),
        "only the last five:\n{}",
        run.out
    );
}

#[cfg(target_os = "macos")]
#[test]
fn status_when_not_loaded_says_so() {
    let sandbox = Sandbox::new();
    let runner = FakeRunner::new(false);
    let env = sandbox.env();
    let run = drive(false, "", |io| status(&env, &runner, io));
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert!(run.out.contains("not running"), "{}", run.out);
    assert!(run.out.contains(sandbox.audit().to_str().unwrap()));
}

#[test]
fn log_prints_the_last_n_lines_default_fifty() {
    let sandbox = Sandbox::new();
    sandbox.write_audit(60);
    let env = sandbox.env();
    let run = drive(false, "", |io| log(&env, 3, io));
    assert_eq!(run.code, 0);
    assert_eq!(run.out.lines().count(), 3, "{}", run.out);
    assert!(run.out.contains("\"n\":60") && run.out.contains("\"n\":58"));
    let run = drive(false, "", |io| log(&env, 50, io));
    assert_eq!(run.out.lines().count(), 50);
    assert!(run.out.lines().next().unwrap().contains("\"n\":11"));
}

#[test]
fn log_without_a_file_says_so() {
    let sandbox = Sandbox::new();
    let env = sandbox.env();
    let run = drive(false, "", |io| log(&env, 50, io));
    assert_eq!(run.code, 0);
    assert!(run.out.contains("no audit log"), "{}", run.out);
}

#[test]
fn run_full_without_confirmation_refuses_with_exit_2() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let output = sandbox.herdr_mcp(&["tunnel", "run", "--full"], None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("herdr-mcp tunnel start --full"));
    assert_eq!(sandbox.recorded("tunnel-client"), "", "must not exec");
}

#[test]
fn run_full_confirmed_passes_control_tools_and_tunnel_mode() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let output = sandbox.herdr_mcp(
        &[
            "tunnel",
            "run",
            "--full",
            "--full-confirmed",
            "--expires-at",
            "1800000000",
            "--session",
            "foo",
        ],
        None,
    );
    assert!(output.status.success(), "{output:?}");
    let rec = sandbox.recorded("tunnel-client");
    assert!(
        rec.contains("--tools control --tunnel --expires-at 1800000000 --session foo"),
        "{rec}"
    );
}

#[test]
fn read_run_never_mentions_control() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let env = sandbox.env();
    let command = prepared_with(
        &env,
        &RunOptions {
            session: Some("foo".into()),
            expires_at: Some(1_800_000_000),
            ..RunOptions::default()
        },
    );
    let line = &args_of(&command)[4];
    assert_eq!(
        *line,
        format!(
            "{} --tools read --tunnel --expires-at 1800000000 --session foo",
            env.exe.display()
        )
    );
    assert!(!line.contains("control"));
    // --full-confirmed alone does not turn on control.
    let command = prepared_with(
        &env,
        &RunOptions {
            full_confirmed: true,
            ..RunOptions::default()
        },
    );
    assert!(!args_of(&command)[4].contains("control"));
}

#[cfg(target_os = "macos")]
#[test]
fn end_to_end_start_status_stop_with_fake_launchctl() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    sandbox.fake_launchctl(false);
    let output = sandbox.herdr_mcp(&["tunnel", "start", "--for", "30m"], None);
    assert!(output.status.success(), "{output:?}");
    assert!(sandbox.plist().exists());
    let text = fs::read_to_string(sandbox.plist()).unwrap();
    let args = plist_args(&text);
    assert_eq!(&args[1..3], ["tunnel", "run"]);
    assert!(args.contains(&"--expires-at".to_string()), "{args:?}");
    let rec = sandbox.recorded("launchctl");
    assert!(rec.contains("bootstrap gui/"), "{rec}");
    assert!(
        rec.contains(&sandbox.plist().display().to_string()),
        "{rec}"
    );

    sandbox.fake_launchctl(true);
    let output = sandbox.herdr_mcp(&["tunnel", "status"], None);
    assert!(output.status.success(), "{output:?}");
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("running") && out.contains("read"), "{out}");

    let output = sandbox.herdr_mcp(&["tunnel", "stop"], None);
    assert!(output.status.success(), "{output:?}");
    assert!(!sandbox.plist().exists());
    assert!(sandbox.recorded("launchctl").contains("bootout gui/"));
}

#[cfg(target_os = "macos")]
#[test]
fn end_to_end_start_full_without_a_tty_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    sandbox.fake_launchctl(false);
    let output = sandbox.herdr_mcp(&["tunnel", "start", "--full"], None);
    assert!(!output.status.success(), "{output:?}");
    assert!(!sandbox.plist().exists());
    assert_eq!(sandbox.recorded("launchctl"), "");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn background_commands_say_macos_only_on_other_platforms() {
    let sandbox = Sandbox::new();
    for command in ["start", "stop", "status"] {
        let output = sandbox.herdr_mcp(&["tunnel", command], None);
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            all.contains("macOS only") && all.contains("tunnel run"),
            "{all}"
        );
    }
}
