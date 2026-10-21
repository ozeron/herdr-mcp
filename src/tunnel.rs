use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::env::{env_nonempty, env_nonempty_string};
use crate::install::{Io, shim_path_in};

pub const KEYCHAIN_SERVICE: &str = "herdr-mcp-tunnel";
pub const KEYCHAIN_ACCOUNT: &str = "runtime-key";
pub const KEY_ENV: &str = "CONTROL_PLANE_API_KEY";
const CLIENT: &str = "tunnel-client";
const FALLBACK_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

#[derive(Debug, Error)]
#[error("{0}")]
pub struct TunnelError(String);

impl TunnelError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Debug, Clone)]
pub struct TunnelEnv {
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
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

fn mcp_command(bridge: &Path, session: Option<&str>) -> String {
    let mut line = format!("{} --tools read", quote(&bridge.display().to_string()));
    if let Some(name) = session {
        line.push_str(&format!(" --session {}", quote(name)));
    }
    line
}

/// The `tunnel-client` invocation. The key goes into the child's environment only.
pub fn build_command(
    client: &Path,
    tunnel_id: &str,
    bridge: &Path,
    session: Option<&str>,
    key: &str,
) -> Command {
    let mut command = Command::new(client);
    command
        .args(["run", "--control-plane.tunnel-id", tunnel_id])
        .arg("--mcp-command")
        .arg(mcp_command(bridge, session))
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

/// Checks everything `tunnel run` needs, prints the banner and returns the command to exec.
pub fn prepare_run(
    env: &TunnelEnv,
    session: Option<&str>,
    io: &mut Io,
) -> Result<Command, TunnelError> {
    let client = env.find_client().ok_or_else(|| {
        TunnelError::new(format!(
            "{CLIENT} not found. Install it with: brew install openai/tools/tunnel-client"
        ))
    })?;
    let id = read_tunnel_id(env)?;
    let key = read_key(env)?;
    let _ = writeln!(
        io.out,
        "ChatGPT can read your Herdr panes while this runs. Read tools only.\nClose this pane or press Ctrl-C to stop."
    );
    let _ = io.out.flush();
    Ok(build_command(&client, &id, &env.bridge(), session, &key))
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
Then open the \"Herdr MCP: ChatGPT tunnel\" pane, or run `herdr-mcp tunnel run`.";

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
