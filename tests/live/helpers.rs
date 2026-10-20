//! Shared plumbing for the live suite: the scratch session, a per-test workspace, and a bridge
//! child driven over stdio.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

pub const SESSION: &str = "herdr-mcp-probe";
pub const BIN: &str = env!("CARGO_BIN_EXE_herdr-mcp");

pub fn enabled() -> bool {
    std::env::var("HERDR_MCP_LIVE").is_ok_and(|value| value == "1")
}

/// Returns early from a test unless the live gate is open.
macro_rules! gate {
    () => {
        if !$crate::helpers::enabled() {
            return;
        }
    };
}
pub(crate) use gate;

/// Working directory for agents; must be a directory the agents already trust.
pub fn agent_cwd() -> PathBuf {
    std::env::var_os("HERDR_MCP_LIVE_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

pub fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{prefix}-{}-{}", std::process::id(), nanos % 100_000_000)
}

fn herdr_command() -> Command {
    let mut command = Command::new("herdr");
    command.args(["--session", SESSION]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            command.env_remove(key);
        }
    }
    command.stdin(Stdio::null());
    command
}

/// Runs `herdr --session herdr-mcp-probe <args>` and returns the JSON `result`.
pub fn herdr(args: &[&str]) -> Value {
    let output = herdr_command().args(args).output().expect("run herdr");
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() && output.status.success() {
        return Value::Null;
    }
    let parsed: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|err| panic!("herdr {args:?}: bad json ({err}); status {}", output.status));
    parsed["result"].clone()
}

pub fn snapshot() -> Value {
    herdr(&["api", "snapshot"])["snapshot"].clone()
}

pub fn pane_count(snapshot: &Value) -> usize {
    snapshot["panes"].as_array().map_or(0, Vec::len)
}

/// `pane_id -> agent_status` for every pane except `skip`.
pub fn statuses_except(snapshot: &Value, skip: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = snapshot["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pane| pane["pane_id"] != skip)
        .map(|pane| {
            (
                pane["pane_id"].as_str().unwrap_or_default().to_string(),
                pane["agent_status"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

pub fn agent_status(snapshot: &Value, pane: &str) -> Option<String> {
    snapshot["panes"]
        .as_array()?
        .iter()
        .find(|p| p["pane_id"] == pane)
        .and_then(|p| p["agent_status"].as_str())
        .map(str::to_string)
}

/// The workspace one test created. Dropping it closes exactly that workspace, also on failure.
pub struct Workspace {
    pub id: String,
    pub pane: String,
}

impl Workspace {
    pub fn create(label: &str) -> Self {
        let cwd = agent_cwd();
        let result = herdr(&[
            "workspace",
            "create",
            "--label",
            label,
            "--cwd",
            cwd.to_str().unwrap(),
            "--no-focus",
        ]);
        Self {
            id: result["workspace"]["workspace_id"]
                .as_str()
                .expect("workspace id")
                .to_string(),
            pane: result["root_pane"]["pane_id"]
                .as_str()
                .expect("root pane id")
                .to_string(),
        }
    }

    /// Types a command into the workspace's root pane, as a human would.
    pub fn type_command(&self, command: &str) {
        herdr(&["pane", "run", &self.pane, command]);
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = herdr_command()
            .args(["workspace", "close", &self.id])
            .output();
    }
}

#[derive(Default)]
pub struct BridgeOpts {
    pub session: Option<String>,
    pub control: bool,
    pub max_wait_ms: Option<u64>,
    pub client: Option<&'static str>,
    pub env: Vec<(String, String)>,
}

impl BridgeOpts {
    pub fn control() -> Self {
        Self {
            session: Some(SESSION.into()),
            control: true,
            ..Self::default()
        }
    }

    pub fn read() -> Self {
        Self {
            session: Some(SESSION.into()),
            ..Self::default()
        }
    }

    fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(session) = &self.session {
            args.extend(["--session".to_string(), session.clone()]);
        }
        if self.control {
            args.extend(["--tools".to_string(), "control".to_string()]);
        }
        if let Some(ms) = self.max_wait_ms {
            args.extend(["--max-wait-ms".to_string(), ms.to_string()]);
        }
        if let Some(client) = self.client {
            args.extend(["--client".to_string(), client.to_string()]);
        }
        args
    }
}

pub struct Reply {
    pub is_error: bool,
    pub body: Value,
}

impl Reply {
    pub fn code(&self) -> Option<&str> {
        self.body["code"].as_str()
    }

    /// Fails the test unless the call succeeded, and no `success` key appears anywhere.
    pub fn ok(self, what: &str) -> Value {
        assert!(!self.is_error, "{what} failed: {}", self.body);
        assert!(
            !has_key(&self.body, "success"),
            "{what} result carries a success field: {}",
            self.body
        );
        self.body
    }
}

fn has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_key(v, key)),
        Value::Array(items) => items.iter().any(|v| has_key(v, key)),
        _ => false,
    }
}

type Client = RunningService<RoleClient, ClientConfig>;

pub struct Bridge {
    client: Client,
    _child: Option<tokio::process::Child>,
}

async fn serve<R, W>(read: R, write: W) -> Client
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("herdr-mcp-live", "1.0.0"),
    )
    .serve((read, write))
    .await
    .expect("MCP initialize")
}

impl Bridge {
    /// Spawns the built binary as an external caller: `HERDR_*` is stripped from its env.
    pub async fn start(opts: BridgeOpts) -> Self {
        let mut command = tokio::process::Command::new(BIN);
        command.args(opts.args());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(key);
            }
        }
        command.envs(opts.env.iter().map(|(k, v)| (k, v)));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn herdr-mcp");
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        Self {
            client: serve(stdout, stdin).await,
            _child: Some(child),
        }
    }

    /// Types the bridge command into `pane`'s shell, so the pane's shell is the bridge's
    /// ancestor and the caller verifies. Stdio goes over two FIFOs.
    pub async fn start_in_pane(pane: &str, dir: &Path, opts: BridgeOpts) -> Self {
        let fifo_in = dir.join("to-bridge");
        let fifo_out = dir.join("from-bridge");
        for fifo in [&fifo_in, &fifo_out] {
            let status = Command::new("mkfifo").arg(fifo).status().expect("mkfifo");
            assert!(status.success());
        }
        let line = format!(
            "{BIN} {} < {} > {}",
            opts.args().join(" "),
            fifo_in.display(),
            fifo_out.display()
        );
        herdr(&["pane", "run", pane, &line]);
        // Open order matches the shell's: input writer first, then output reader.
        let write = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().write(true).open(fifo_in)
        })
        .await
        .unwrap()
        .expect("open input fifo");
        let read = tokio::task::spawn_blocking(move || std::fs::File::open(fifo_out))
            .await
            .unwrap()
            .expect("open output fifo");
        Self {
            client: serve(
                tokio::fs::File::from_std(read),
                tokio::fs::File::from_std(write),
            )
            .await,
            _child: None,
        }
    }

    pub async fn call(&self, tool: &'static str, args: Value) -> Reply {
        let mut params = CallToolRequestParams::new(tool);
        params.arguments = args.as_object().cloned();
        let result = self
            .client
            .call_tool(params)
            .await
            .unwrap_or_else(|err| panic!("{tool}: transport error {err}"));
        let body = result
            .structured_content
            .clone()
            .unwrap_or_else(|| serde_json::to_value(&result.content).unwrap_or(Value::Null));
        Reply {
            is_error: result.is_error == Some(true),
            body,
        }
    }

    pub async fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .client
            .list_all_tools()
            .await
            .expect("tools/list")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        names
    }

    pub async fn close(self) {
        let _ = self.client.cancel().await;
    }
}

/// Re-waits on `wait_args` while it times out, up to `budget`.
pub async fn wait_until_matched(bridge: &Bridge, wait_args: Value, budget: Duration) -> Value {
    let start = Instant::now();
    loop {
        let body = bridge
            .call("herdr_wait", wait_args.clone())
            .await
            .ok("herdr_wait");
        if body["matched"] == true {
            return body;
        }
        assert!(
            body["timed_out"] == true && start.elapsed() < budget,
            "wait did not match within budget: {body}"
        );
    }
}

pub async fn read_text(bridge: &Bridge, target: &str, lines: u32) -> String {
    bridge
        .call("herdr_read", json!({"target": target, "lines": lines}))
        .await
        .ok("herdr_read")["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Polls the pane until `needle` shows up (case-insensitive) or `budget` runs out.
pub async fn read_until(
    bridge: &Bridge,
    target: &str,
    needle: &str,
    budget: Duration,
) -> Option<String> {
    let start = Instant::now();
    loop {
        let text = read_text(bridge, target, 120).await;
        if text.to_lowercase().contains(&needle.to_lowercase()) {
            return Some(text);
        }
        if start.elapsed() > budget {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
