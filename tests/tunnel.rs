use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::Duration;

use herdr_mcp::install::Io;
use herdr_mcp::tunnel::{
    DoctorOptions, JobState, RunOptions, Runner, StartOptions, TunnelEnv, doctor, log,
    parse_duration, parse_job_state, prepare_run, setup, start, status, stop, validate_tunnel_id,
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

const SETUP_KEY: &str = "sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abcdefgh";

impl Sandbox {
    /// A fake `security`: records argv and the stdin of `-i`, and answers `find-generic-password`
    /// with `readback`, or with the key it was last given when `readback` is `None`.
    fn fake_security(&self, readback: Option<&str>) {
        let rec = self.record("security");
        let stdin = self.root.join("security.stdin");
        let stored = self.root.join("security.stored");
        let find = match readback {
            Some(text) => format!("printf '%s\\n' '{text}'"),
            None => format!("cat '{}'", stored.display()),
        };
        let script = format!(
            "#!/bin/sh\necho \"argv:$*\" >> '{rec}'\ncase \"$1\" in\n-i) cat >> '{stdin}'; sed -n 's/.* -w \"\\(.*\\)\"$/\\1/p' '{stdin}' | tail -n 1 > '{stored}' ;;\nfind-generic-password) {find} ;;\nesac\nexit 0\n",
            rec = rec.display(),
            stdin = stdin.display(),
            stored = stored.display(),
        );
        let path = self.bin().join("security");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn security_stdin(&self) -> String {
        fs::read_to_string(self.root.join("security.stdin")).unwrap_or_default()
    }
}

fn setup_input(key: &str) -> String {
    format!("{ID}\n{key}\n")
}

#[test]
fn setup_stores_the_key_via_security_and_saves_the_id_privately() {
    let sandbox = Sandbox::new();
    sandbox.fake_security(None);
    let env = sandbox.env();
    let run = drive(true, &setup_input(SETUP_KEY), |io| setup(&env, io));
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert_eq!(
        sandbox.recorded("security"),
        "argv:-i\nargv:find-generic-password -s herdr-mcp-tunnel -a runtime-key -w\n"
    );
    assert_eq!(
        sandbox.security_stdin(),
        format!("add-generic-password -U -s herdr-mcp-tunnel -a runtime-key -w \"{SETUP_KEY}\"\n")
    );
    assert!(!sandbox.recorded("security").contains(SETUP_KEY));
    let path = sandbox.root.join("xdg/herdr-mcp/tunnel");
    assert_eq!(fs::read_to_string(&path).unwrap().trim(), ID);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(run.out.contains("Runtime key (sk-...): "), "{}", run.out);
    assert!(!run.out.contains("when the Keychain asks"), "{}", run.out);
    assert!(run.out.contains("goes into the Keychain"), "{}", run.out);
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
fn setup_stores_a_164_character_key_whole_and_never_prints_it() {
    let sandbox = Sandbox::new();
    sandbox.fake_security(None);
    let env = sandbox.env();
    let key = format!("sk-proj-{}", "aB3_-x".repeat(26));
    assert_eq!(key.len(), 164);
    let run = drive(true, &setup_input(&format!("  {key}  ")), |io| {
        setup(&env, io)
    });
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert!(
        sandbox
            .security_stdin()
            .contains(&format!("-w \"{key}\"\n"))
    );
    assert!(!run.out.contains(&key) && !run.err.contains(&key));
    assert!(!sandbox.recorded("security").contains(&key));
}

#[test]
fn setup_rejects_a_key_that_is_short_or_has_a_space_or_a_quote_before_any_security_call() {
    let long = "x".repeat(60);
    let bad = [
        "sk-short".to_owned(),
        format!("sk-{}", "a".repeat(36)),
        format!("pk-{long}"),
        format!("sk-{long} {long}"),
        format!("sk-{long}\"; delete-generic-password"),
        format!("sk-{long}'"),
        String::new(),
    ];
    for key in bad {
        let sandbox = Sandbox::new();
        sandbox.fake_security(None);
        let env = sandbox.env();
        let run = drive(true, &setup_input(&key), |io| setup(&env, io));
        assert_eq!(run.code, 1, "{key:?}: {}{}", run.out, run.err);
        assert!(run.err.contains("paste the whole key"), "{}", run.err);
        assert!(!run.err.contains(&long) && !run.out.contains(&long));
        assert_eq!(sandbox.recorded("security"), "", "{key:?}");
        assert!(!sandbox.root.join("xdg/herdr-mcp/tunnel").exists());
    }
}

#[test]
fn setup_fails_when_the_keychain_reads_back_a_different_key() {
    let sandbox = Sandbox::new();
    sandbox.fake_security(Some(&SETUP_KEY[..40]));
    let env = sandbox.env();
    let run = drive(true, &setup_input(SETUP_KEY), |io| setup(&env, io));
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("the Keychain stored a different key"),
        "{}",
        run.err
    );
    let all = format!("{}{}", run.out, run.err);
    assert!(
        !all.contains(SETUP_KEY) && !all.contains(&SETUP_KEY[..40]),
        "{all}"
    );
    assert!(!sandbox.root.join("xdg/herdr-mcp/tunnel").exists());
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

    assert_eq!(runner.verbs(), ["print", "bootstrap", "print"]);
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
    assert_eq!(runner.verbs(), ["print", "bootout", "bootstrap", "print"]);
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

/// What `launchctl print` showed in the smoke test for a job that keeps exiting.
const PRINT_FAILING: &str = "gui/501/dev.herdr-mcp.tunnel = {\n\tactive count = 0\n\tstate = spawn scheduled\n\tprogram = /x/herdr-mcp\n\truns = 2\n\tlast exit code = 1\n\tendpoints = {\n\t\t\"x\" = {\n\t\t\tstate = active\n\t\t}\n\t}\n}\n";
const PRINT_RUNNING: &str = "gui/501/dev.herdr-mcp.tunnel = {\n\tstate = running\n\truns = 1\n\tlast exit code = (never exited)\n\tendpoints = {\n\t\t\"x\" = {\n\t\t\tstate = active\n\t\t}\n\t}\n}\n";

#[test]
fn job_state_reads_the_jobs_own_lines_not_nested_ones() {
    assert_eq!(parse_job_state(PRINT_RUNNING), JobState::Running);
    assert_eq!(parse_job_state(PRINT_FAILING), JobState::Failing(1));
    assert_eq!(
        parse_job_state("x = {\n\tstate = waiting\n\tlast exit code = 0\n}\n"),
        JobState::Starting
    );
    assert_eq!(
        parse_job_state("x = {\n\tstate = spawn scheduled\n\tlast exit code = (never exited)\n}\n"),
        JobState::Starting
    );
    assert_eq!(parse_job_state(""), JobState::Loaded);
}

/// A launchd whose job dies at once: `print` reports it failing, and bootstrap leaves an error
/// in the service log the way the real job would.
struct DyingRunner {
    err_log: PathBuf,
    loaded: Mutex<bool>,
    booted_out: Mutex<bool>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl Runner for DyingRunner {
    fn run(&self, _program: &Path, args: &[String]) -> std::io::Result<i32> {
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            "bootstrap" => {
                *self.loaded.lock().unwrap() = true;
                fs::write(
                    &self.err_log,
                    "herdr-mcp: no runtime key: set CONTROL_PLANE_API_KEY or run `herdr-mcp tunnel setup`\n",
                )?;
                Ok(0)
            }
            "bootout" => {
                *self.loaded.lock().unwrap() = false;
                *self.booted_out.lock().unwrap() = true;
                Ok(0)
            }
            "print" if !*self.loaded.lock().unwrap() => Ok(113),
            _ => Ok(0),
        }
    }

    fn run_output(&self, program: &Path, args: &[String]) -> std::io::Result<(i32, String)> {
        let code = self.run(program, args)?;
        let out = if args[0] == "print" && code == 0 {
            PRINT_FAILING.to_owned()
        } else {
            String::new()
        };
        Ok((code, out))
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_job_that_dies_on_start_is_unloaded_and_its_error_shown() {
    let sandbox = Sandbox::new();
    sandbox.ready();
    let runner = DyingRunner {
        err_log: sandbox.state_dir().join("tunnel.err.log"),
        loaded: Mutex::new(false),
        booted_out: Mutex::new(false),
        calls: Mutex::new(Vec::new()),
    };
    let env = sandbox.env();
    let run = drive(false, "", |io| {
        start(&env, &StartOptions::default(), &runner, NOW, io)
    });
    assert_ne!(run.code, 0, "{}", run.err);
    assert!(
        run.err.contains("exited right away (exit 1)"),
        "{}",
        run.err
    );
    assert!(run.err.contains("no runtime key"), "{}", run.err);
    assert!(
        *runner.booted_out.lock().unwrap(),
        "the dying job is booted out"
    );
    assert!(!sandbox.plist().exists(), "no plist is left behind");
    assert!(!sandbox.state_dir().join("tunnel.state").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn status_reports_a_failing_job_with_its_last_error() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_dir()).unwrap();
    let runner = DyingRunner {
        err_log: sandbox.state_dir().join("tunnel.err.log"),
        loaded: Mutex::new(true),
        booted_out: Mutex::new(false),
        calls: Mutex::new(Vec::new()),
    };
    fs::write(&runner.err_log, "first\nherdr-mcp: no runtime key\n").unwrap();
    let env = sandbox.env();
    let run = drive(false, "", |io| status(&env, &runner, io));
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out.contains("service:   failing (last exit 1") && run.out.contains("no runtime key"),
        "{}",
        run.out
    );
}

// ---- doctor ----

const LONG_KEY_BODY: &str = "aB3_-x";

fn proj_key(len: usize) -> String {
    let body = len - "sk-proj-".len();
    format!(
        "sk-proj-{}",
        LONG_KEY_BODY.repeat(body / 6 + 1)[..body].to_owned()
    )
}

const RUNNING: &str =
    "gui/501/dev.herdr-mcp.tunnel = {\n\tstate = running\n\tlast exit code = (never exited)\n}\n";

/// Answers `launchctl print` with `print`, or as not loaded when that is `None`.
struct PrintRunner(Option<&'static str>);

impl Runner for PrintRunner {
    fn run(&self, _program: &Path, _args: &[String]) -> std::io::Result<i32> {
        Ok(i32::from(self.0.is_none()))
    }

    fn run_output(&self, program: &Path, args: &[String]) -> std::io::Result<(i32, String)> {
        let code = self.run(program, args)?;
        Ok((code, self.0.unwrap_or_default().to_owned()))
    }
}

impl Sandbox {
    /// A fake `tunnel-client`: `--version` prints `version`; `run` records argv, the pid and
    /// whether the key is in its env, prints `lines`, then stays up until killed.
    fn fake_client(&self, version: &str, lines: &[&str]) {
        let rec = self.record("tunnel-client");
        let pid = self.root.join("tunnel-client.pid");
        let printed: String = lines
            .iter()
            .map(|line| format!("printf '%s\\n' '{line}'\n"))
            .collect();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; echo second line; exit 0; fi\n{{ echo \"argv:$*\"; if [ -n \"${{CONTROL_PLANE_API_KEY+x}}\" ]; then echo key:set; else echo key:unset; fi; }} >> '{rec}'\necho $$ > '{pid}'\n{printed}exec sleep 30\n",
            rec = rec.display(),
            pid = pid.display()
        );
        let path = self.bin().join("tunnel-client");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn probe_pid(&self) -> Option<i32> {
        fs::read_to_string(self.root.join("tunnel-client.pid"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    }

    fn write_err_log(&self, lines: &[&str]) {
        fs::create_dir_all(self.state_dir()).unwrap();
        fs::write(
            self.state_dir().join("tunnel.err.log"),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    fn doctor_env(&self) -> TunnelEnv {
        let mut env = self.env();
        env.exe = PathBuf::from(env!("CARGO_BIN_EXE_herdr-mcp"));
        env
    }

    /// Everything present and healthy as far as the fakes go.
    fn doctor_ready(&self, key: &str) {
        self.fake_client("tunnel-client 1.2.3", &[]);
        self.fake_security(Some(key));
        self.save_id(ID);
    }
}

fn options(probe_ms: u64) -> DoctorOptions {
    DoctorOptions {
        probe_timeout: Duration::from_millis(probe_ms),
        ..DoctorOptions::default()
    }
}

fn run_doctor(env: &TunnelEnv, runner: &dyn Runner, options: &DoctorOptions) -> Run {
    drive(false, "", |io| doctor(env, runner, options, io))
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A localhost server for the two health GETs the doctor makes.
fn serve_health(mcp_body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
            }
            let body = if request.starts_with(b"GET /readyz") {
                "ready"
            } else {
                mcp_body
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

fn has_line(out: &str, tag: &str, needle: &str) -> bool {
    out.lines()
        .any(|line| line.starts_with(tag) && line.contains(needle))
}

#[test]
fn doctor_checks_client_id_and_key_and_never_prints_the_key() {
    let sandbox = Sandbox::new();
    let key = proj_key(164);
    sandbox.doctor_ready(&key);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(
        has_line(&run.out, "PASS", "tunnel-client 1.2.3 ("),
        "{}",
        run.out
    );
    assert!(!run.out.contains("second line"), "{}", run.out);
    assert!(
        has_line(&run.out, "PASS", &format!("tunnel ID: {ID}")),
        "{}",
        run.out
    );
    assert!(
        has_line(&run.out, "PASS", "Keychain, sk-proj- key, 164 characters"),
        "{}",
        run.out
    );
    assert!(!run.out.contains(&key) && !run.err.contains(&key));
}

#[test]
fn doctor_warns_when_the_key_is_exactly_128_characters() {
    let sandbox = Sandbox::new();
    let key = proj_key(128);
    sandbox.doctor_ready("unused");
    let mut env = sandbox.doctor_env();
    env.api_key = Some(key.clone());
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(
        has_line(
            &run.out,
            "WARN",
            "CONTROL_PLANE_API_KEY, sk-proj- key, 128 characters; looks truncated"
        ),
        "{}",
        run.out
    );
    assert!(
        run.out.contains("fix: rerun `herdr-mcp tunnel setup`"),
        "{}",
        run.out
    );
    assert!(!run.out.contains(&key));
}

#[test]
fn doctor_fails_with_fixes_when_client_id_and_key_are_missing() {
    let sandbox = Sandbox::new();
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(None), &options(100));
    assert_eq!(run.code, 1, "{}", run.out);
    assert!(
        has_line(&run.out, "FAIL", "tunnel-client: not found"),
        "{}",
        run.out
    );
    assert!(
        run.out
            .contains("fix: brew install openai/tools/tunnel-client")
    );
    assert!(
        has_line(&run.out, "FAIL", "tunnel ID: no tunnel ID saved"),
        "{}",
        run.out
    );
    assert!(
        has_line(&run.out, "FAIL", "runtime key: none"),
        "{}",
        run.out
    );
    assert!(run.out.contains("fix: run `herdr-mcp tunnel setup`"));
    assert!(
        has_line(&run.out, "WARN", "OpenAI key check: skipped"),
        "{}",
        run.out
    );
}

#[test]
fn doctor_asks_the_real_bridge_for_its_tools() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(has_line(&run.out, "PASS", "answers MCP, "), "{}", run.out);
    assert!(run.out.contains(" read tools"), "{}", run.out);
    // The bridge got a scratch HOME and state dir, so it left nothing in the sandbox's.
    assert!(!sandbox.home().join(".config").exists());
    assert!(!sandbox.state_dir().exists());
}

#[test]
fn doctor_fails_when_the_bridge_does_not_exist() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let env = sandbox.env();
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(
        has_line(
            &run.out,
            "FAIL",
            "bridge: /opt/herdr/bin/herdr-mcp does not exist"
        ),
        "{}",
        run.out
    );
    assert_eq!(run.code, 1);
}

#[test]
fn doctor_reads_a_newest_401_in_the_log_as_a_rejected_key() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    sandbox.write_err_log(&[
        "tunnel-client started",
        "tunnel metadata fetched",
        "poll failed: 401 invalid_api_key",
    ]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(Some(RUNNING)), &options(1500));
    assert_eq!(run.code, 1, "{}", run.out);
    assert!(
        has_line(&run.out, "FAIL", "OpenAI rejected the key (401)"),
        "{}",
        run.out
    );
    assert_eq!(
        sandbox.recorded("tunnel-client"),
        "",
        "no probe while the service runs"
    );
}

#[test]
fn doctor_reads_a_newest_fetch_in_the_log_as_an_accepted_key() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    sandbox.write_err_log(&[
        "poll failed: 401 invalid_api_key",
        "listening on http://127.0.0.1:40123",
        "tunnel metadata fetched",
    ]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(Some(RUNNING)), &options(1500));
    assert!(
        has_line(
            &run.out,
            "PASS",
            "OpenAI accepted the key; tunnel connected"
        ),
        "{}",
        run.out
    );
    assert_eq!(sandbox.recorded("tunnel-client"), "");
}

#[test]
fn a_port_that_contains_401_is_not_an_auth_failure() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    sandbox.write_err_log(&[
        "tunnel-client started",
        "listening on http://127.0.0.1:54012",
    ]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(Some(RUNNING)), &options(1500));
    assert!(
        has_line(&run.out, "PASS", "OpenAI accepted the key"),
        "{}",
        run.out
    );
}

#[test]
fn doctor_without_a_service_probes_and_passes_on_a_metadata_line_then_kills_it() {
    let sandbox = Sandbox::new();
    let key = proj_key(164);
    sandbox.doctor_ready(&key);
    sandbox.fake_client("tunnel-client 1.2.3", &["boot", "tunnel metadata fetched"]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(
        has_line(
            &run.out,
            "PASS",
            "OpenAI accepted the key; tunnel metadata fetched"
        ),
        "{}",
        run.out
    );
    assert!(run.out.contains("briefly takes the tunnel"), "{}", run.out);
    let rec = sandbox.recorded("tunnel-client");
    assert!(
        rec.contains(&format!(
            "argv:run --embedded-stateless-mcp-stub --control-plane.tunnel-id {ID} --health.listen-addr 127.0.0.1:0\nkey:set"
        )),
        "{rec}"
    );
    assert!(!rec.contains(&key) && !run.out.contains(&key));
    let pid = sandbox.probe_pid().expect("probe ran");
    assert!(!alive(pid), "the probe must be killed");
}

#[test]
fn doctor_probe_fails_on_401_and_403_and_warns_on_a_timeout() {
    let cases = [
        (
            "tunnel-client started",
            "HTTP 401 invalid_api_key",
            "FAIL",
            "rejected the key (401 invalid_api_key)",
            1,
        ),
        (
            "tunnel-client started",
            "403 Forbidden",
            "FAIL",
            "the key lacks Tunnels Read+Use, or the tunnel is in another org or workspace",
            1,
        ),
        (
            "x",
            "insufficient permission for tunnel",
            "FAIL",
            "the key lacks Tunnels Read+Use",
            1,
        ),
    ];
    for (_, line, tag, text, code) in cases {
        let sandbox = Sandbox::new();
        sandbox.doctor_ready(&proj_key(164));
        sandbox.fake_client("tunnel-client 1.2.3", &[line]);
        let env = sandbox.doctor_env();
        let run = run_doctor(&env, &PrintRunner(None), &options(1500));
        assert!(has_line(&run.out, tag, text), "{line}: {}", run.out);
        assert_eq!(run.code, code);
        assert!(!alive(sandbox.probe_pid().unwrap()));
    }

    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let env = sandbox.doctor_env();
    let start = std::time::Instant::now();
    let run = run_doctor(&env, &PrintRunner(None), &options(300));
    assert!(start.elapsed() < Duration::from_secs(8));
    assert!(
        has_line(&run.out, "WARN", "no answer from OpenAI within 300ms"),
        "{}",
        run.out
    );
    assert!(
        !alive(sandbox.probe_pid().unwrap()),
        "a timed-out probe is killed"
    );
}

#[test]
fn doctor_probe_reports_a_client_that_exits_early() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let script =
        "#!/bin/sh\n[ \"$1\" = --version ] && echo v1 && exit 0\necho 'unknown flag' >&2\nexit 3\n";
    let path = sandbox.bin().join("tunnel-client");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(
        has_line(
            &run.out,
            "FAIL",
            "tunnel-client exited (code 3) before connecting: unknown flag"
        ),
        "{}",
        run.out
    );
}

#[test]
fn doctor_reports_the_service_state_and_warns_when_it_is_not_running() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(None), &options(1500));
    assert!(
        has_line(&run.out, "WARN", "service: not loaded"),
        "{}",
        run.out
    );
    assert!(run.out.contains("fix: run `herdr-mcp tunnel start`"));
    let failing = "\tstate = waiting\n\tlast exit code = 1\n";
    sandbox.write_err_log(&["herdr-mcp: boom"]);
    let run = run_doctor(&env, &PrintRunner(Some(failing)), &options(1500));
    assert!(
        has_line(
            &run.out,
            "WARN",
            "service: failing (last exit 1): herdr-mcp: boom"
        ),
        "{}",
        run.out
    );
}

#[test]
fn doctor_checks_the_tunnels_health_endpoints_when_the_service_runs() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let port = serve_health(
        r#"{"component":"mcp","status":"ok","state":"observed","details":{"channel":"main","transport":"stdio","initialize":{"ok":true},"tools_list":{"ok":true,"tool_names":["a","b","c","d","e","f","g"]}}}"#,
    );
    sandbox.write_err_log(&[
        "tunnel metadata fetched",
        &format!("health listening on http://127.0.0.1:{port}"),
    ]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(Some(RUNNING)), &options(1500));
    assert!(
        has_line(&run.out, "PASS", "service: running"),
        "{}",
        run.out
    );
    assert!(
        has_line(&run.out, "PASS", &format!("/readyz on port {port}")),
        "{}",
        run.out
    );
    assert!(
        has_line(&run.out, "PASS", "initialize ok, tools/list ok, 7 tools"),
        "{}",
        run.out
    );
    assert_eq!(run.code, 0, "{}", run.out);
}

#[test]
fn doctor_treats_not_observed_as_fine() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    // The shape tunnel-client 0.0.16 returned before any ChatGPT call.
    let port = serve_health(
        r#"{"schema_version":1,"component":"mcp","status":"unknown","state":"not_observed","limited":false,"details":{"channel":"main","transport":"stdio","child_state":"running","initialize":{"ok":false,"identity_complete":false,"limited":false,"capability_names":[]},"tools_list":{"ok":false,"tool_names":[],"retained_count":0,"complete":false}}}"#,
    );
    sandbox.write_err_log(&[
        "tunnel metadata fetched",
        &format!("health on http://127.0.0.1:{port}"),
    ]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(Some(RUNNING)), &options(1500));
    assert!(
        has_line(
            &run.out,
            "PASS",
            "initialize not observed, tools/list not observed (ChatGPT has not called yet)"
        ),
        "{}",
        run.out
    );
}

#[test]
fn doctor_fails_when_the_health_port_is_closed() {
    let sandbox = Sandbox::new();
    sandbox.doctor_ready(&proj_key(164));
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    sandbox.write_err_log(&[
        "tunnel metadata fetched",
        &format!("health on http://127.0.0.1:{closed}"),
    ]);
    let env = sandbox.doctor_env();
    let run = run_doctor(&env, &PrintRunner(Some(RUNNING)), &options(1500));
    assert!(has_line(&run.out, "FAIL", "/readyz"), "{}", run.out);
    assert_eq!(run.code, 1);
}

#[test]
fn status_flags_a_401_as_the_newest_auth_line_while_the_service_runs() {
    let sandbox = Sandbox::new();
    sandbox.write_err_log(&[
        "tunnel metadata fetched",
        "poll failed: 401 invalid_api_key",
    ]);
    let env = sandbox.env();
    let run = drive(false, "", |io| {
        status(&env, &PrintRunner(Some(RUNNING)), io)
    });
    assert!(
        run.out
            .contains("auth:      FAILING (401 invalid_api_key) - run herdr-mcp tunnel doctor"),
        "{}",
        run.out
    );

    sandbox.write_err_log(&[
        "poll failed: 401 invalid_api_key",
        "tunnel metadata fetched",
    ]);
    let run = drive(false, "", |io| {
        status(&env, &PrintRunner(Some(RUNNING)), io)
    });
    assert!(!run.out.contains("auth:"), "{}", run.out);

    sandbox.write_err_log(&["poll failed: 401 invalid_api_key"]);
    let run = drive(false, "", |io| status(&env, &PrintRunner(None), io));
    assert!(!run.out.contains("auth:"), "not loaded: {}", run.out);
}

#[test]
fn end_to_end_doctor_exits_zero_when_nothing_fails_and_non_zero_otherwise() {
    let sandbox = Sandbox::new();
    sandbox.fake_launchctl(false);
    sandbox.fake_client("tunnel-client 1.2.3", &["tunnel metadata fetched"]);
    sandbox.fake_security(Some(&proj_key(164)));
    sandbox.save_id(ID);
    let output = sandbox.herdr_mcp(&["tunnel", "doctor"], None);
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        out.contains("PASS  tunnel-client: tunnel-client 1.2.3"),
        "{out}"
    );

    let empty = Sandbox::new();
    empty.fake_launchctl(false);
    let output = empty.herdr_mcp(&["tunnel", "doctor"], None);
    assert_eq!(output.status.code(), Some(1));
}
