use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use herdr_mcp::install::Io;
use herdr_mcp::tunnel::{TunnelEnv, prepare_run, setup, validate_tunnel_id};
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
        for dir in ["home", "bin", "xdg"] {
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
    let mut command = None;
    let run = drive(false, "", |io| {
        command = prepare_run(env, session, io).ok();
        0
    });
    assert!(run.out.contains("Read tools only"), "{}", run.out);
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
            &format!("{} --tools read", env.exe.display()),
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
        format!("\"{}\" --tools read", shim.display())
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
        format!("{} --tools read --session foo", env.exe.display())
    );
}

#[test]
fn missing_tunnel_client_names_the_brew_command() {
    let sandbox = Sandbox::new();
    sandbox.save_id(ID);
    let env = sandbox.env();
    let mut err = None;
    drive(false, "", |io| {
        err = prepare_run(&env, None, io).err().map(|e| e.to_string());
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
        err = prepare_run(&env, None, io).err().map(|e| e.to_string());
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
    assert!(rec.contains("--tools read --session foo"), "{rec}");
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
