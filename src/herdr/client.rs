use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, LinesCodec, LinesCodecError};

use super::error::{ApiErrorCode, HerdrError};
use super::events::Subscription;
use super::types::*;

pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// macOS `sun_path` holds 104 bytes including the terminating NUL.
pub const MAX_SOCKET_PATH_BYTES: usize = 103;
/// Bound on connecting, writing a request and reading each reply.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Added to a server-side wait, so Herdr's own timeout reply arrives before ours fires.
pub const WAIT_MARGIN: Duration = Duration::from_secs(5);
pub const MIN_HERDR_VERSION: (u64, u64, u64) = (0, 9, 3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T> {
    Done(T),
    TimedOut { message: String },
}

enum Reply {
    Result(Box<ResponseResult>),
    Timeout(String),
}

#[derive(Debug)]
pub struct Client {
    socket_path: PathBuf,
    next_id: AtomicU64,
    timeout: Duration,
}

pub(crate) struct LineReader {
    stream: UnixStream,
    buf: BytesMut,
    codec: LinesCodec,
    eof: bool,
}

impl LineReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buf: BytesMut::new(),
            codec: LinesCodec::new_with_max_length(MAX_LINE_BYTES),
            eof: false,
        }
    }

    pub(crate) async fn next_line(&mut self) -> Result<Option<String>, HerdrError> {
        loop {
            if let Some(line) = self.codec.decode(&mut self.buf).map_err(map_codec_error)? {
                return Ok(Some(line));
            }
            if self.eof {
                return Ok(None);
            }
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                self.eof = true;
                return self
                    .codec
                    .decode_eof(&mut self.buf)
                    .map_err(map_codec_error);
            }
        }
    }
}

fn map_codec_error(err: LinesCodecError) -> HerdrError {
    match err {
        LinesCodecError::MaxLineLengthExceeded => HerdrError::LineTooLong {
            limit: MAX_LINE_BYTES,
        },
        LinesCodecError::Io(err) => HerdrError::Io(err),
    }
}

impl Client {
    pub fn new(socket_path: impl AsRef<Path>) -> Result<Self, HerdrError> {
        let path = socket_path.as_ref();
        let len = path.as_os_str().len();
        if len > MAX_SOCKET_PATH_BYTES {
            return Err(HerdrError::SocketPathTooLong {
                path: path.into(),
                len,
                max: MAX_SOCKET_PATH_BYTES,
            });
        }
        Ok(Self {
            socket_path: path.into(),
            next_id: AtomicU64::new(1),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The bound on one socket step: connect, write, or one reply read.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn connect(&self) -> Result<UnixStream, HerdrError> {
        within(
            self.timeout,
            "accept the connection",
            UnixStream::connect(&self.socket_path),
        )
        .await?
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                HerdrError::NotRunning {
                    path: self.socket_path.clone(),
                }
            }
            _ => HerdrError::Io(err),
        })
    }

    pub(crate) async fn send(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<LineReader, HerdrError> {
        let id = format!("hm-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut line =
            serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))
                .map_err(|err| HerdrError::protocol(format!("cannot encode {method}: {err}")))?;
        line.push(b'\n');
        let mut stream = self.connect().await?;
        within(self.timeout, "accept the request", async {
            stream.write_all(&line).await?;
            stream.flush().await
        })
        .await??;
        Ok(LineReader::new(stream))
    }

    /// The reply bound for a call Herdr holds open for `wait_ms` (or its default).
    fn wait_bound(&self, wait_ms: Option<u64>) -> Duration {
        wait_ms.map_or(self.timeout, Duration::from_millis) + WAIT_MARGIN
    }

    async fn call_reply(
        &self,
        method: &str,
        params: impl Serialize,
        reply_timeout: Duration,
    ) -> Result<Reply, HerdrError> {
        let mut reader = self.send(method, params).await?;
        let line = within(reply_timeout, "reply", reader.next_line())
            .await??
            .ok_or_else(|| {
                HerdrError::protocol(format!("connection closed without a response to {method}"))
            })?;
        parse_response(method, &line)
    }

    async fn call<T>(
        &self,
        method: &str,
        params: impl Serialize,
        pick: impl FnOnce(ResponseResult) -> Option<T>,
    ) -> Result<T, HerdrError> {
        match self.call_reply(method, params, self.timeout).await? {
            Reply::Result(result) => pick_result(method, *result, pick),
            Reply::Timeout(message) => Err(HerdrError::Api {
                code: ApiErrorCode::Timeout,
                message,
            }),
        }
    }

    async fn call_outcome<T>(
        &self,
        method: &str,
        params: impl Serialize,
        reply_timeout: Duration,
        pick: impl FnOnce(ResponseResult) -> Option<T>,
    ) -> Result<Outcome<T>, HerdrError> {
        match self.call_reply(method, params, reply_timeout).await? {
            Reply::Result(result) => pick_result(method, *result, pick).map(Outcome::Done),
            Reply::Timeout(message) => Ok(Outcome::TimedOut { message }),
        }
    }

    pub async fn ping(&self) -> Result<Pong, HerdrError> {
        self.call("ping", json!({}), |r| match r {
            ResponseResult::Pong {
                version,
                protocol,
                capabilities,
            } => Some(Pong {
                version,
                protocol,
                capabilities,
            }),
            _ => None,
        })
        .await
    }

    pub async fn check_compat(&self) -> Result<Pong, HerdrError> {
        let pong = self.ping().await?;
        let parsed = parse_version(&pong.version).ok_or_else(|| {
            HerdrError::incompatible(format!("unparseable herdr version {:?}", pong.version))
        })?;
        if parsed < MIN_HERDR_VERSION {
            let (a, b, c) = MIN_HERDR_VERSION;
            return Err(HerdrError::incompatible(format!(
                "herdr {} is older than the required {a}.{b}.{c}",
                pong.version
            )));
        }
        Ok(pong)
    }

    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, HerdrError> {
        self.call("session.snapshot", json!({}), |r| match r {
            ResponseResult::SessionSnapshot { snapshot } => Some(*snapshot),
            _ => None,
        })
        .await
    }

    pub async fn agent_manifests(&self) -> Result<Vec<AgentManifestInfo>, HerdrError> {
        self.call("server.agent_manifests", json!({}), |r| match r {
            ResponseResult::AgentManifestStatus { manifests } => Some(manifests),
            _ => None,
        })
        .await
    }

    pub async fn agent_get(&self, target: &str) -> Result<AgentInfo, HerdrError> {
        self.call("agent.get", json!({"target": target}), |r| match r {
            ResponseResult::AgentInfo { agent } => Some(agent),
            _ => None,
        })
        .await
    }

    pub async fn agent_list(&self) -> Result<Vec<AgentInfo>, HerdrError> {
        self.call("agent.list", json!({}), |r| match r {
            ResponseResult::AgentList { agents } => Some(agents),
            _ => None,
        })
        .await
    }

    pub async fn agent_read(&self, params: &AgentReadParams) -> Result<PaneReadResult, HerdrError> {
        self.call("agent.read", params, pick_read).await
    }

    pub async fn agent_start(
        &self,
        params: &AgentStartParams,
    ) -> Result<Outcome<AgentStarted>, HerdrError> {
        let bound = self.wait_bound(params.timeout_ms);
        self.call_outcome("agent.start", params, bound, |r| match r {
            ResponseResult::AgentStarted(started) => Some(started),
            _ => None,
        })
        .await
    }

    pub async fn agent_prompt(
        &self,
        params: &AgentPromptParams,
    ) -> Result<Outcome<AgentInfo>, HerdrError> {
        let bound = match &params.wait {
            Some(wait) => self.wait_bound(wait.timeout_ms),
            None => self.timeout,
        };
        self.call_outcome("agent.prompt", params, bound, |r| match r {
            ResponseResult::AgentPrompted { agent } => Some(agent),
            _ => None,
        })
        .await
    }

    pub async fn agent_send_keys(&self, target: &str, keys: &[String]) -> Result<(), HerdrError> {
        self.call(
            "agent.send_keys",
            json!({"target": target, "keys": keys}),
            pick_ok,
        )
        .await
    }

    pub async fn agent_explain(&self, target: &str) -> Result<Value, HerdrError> {
        self.call("agent.explain", json!({"target": target}), |r| match r {
            ResponseResult::AgentExplain { explain } => Some(explain),
            _ => None,
        })
        .await
    }

    pub async fn pane_read(&self, params: &PaneReadParams) -> Result<PaneReadResult, HerdrError> {
        self.call("pane.read", params, pick_read).await
    }

    pub async fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError> {
        self.call(
            "pane.process_info",
            json!({"pane_id": pane_id}),
            |r| match r {
                ResponseResult::PaneProcessInfo { process_info } => Some(process_info),
                _ => None,
            },
        )
        .await
    }

    pub async fn pane_send_input(&self, params: &PaneSendInputParams) -> Result<(), HerdrError> {
        self.call("pane.send_input", params, pick_ok).await
    }

    pub async fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<(), HerdrError> {
        self.call(
            "pane.send_text",
            json!({"pane_id": pane_id, "text": text}),
            pick_ok,
        )
        .await
    }

    pub async fn pane_send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), HerdrError> {
        self.call(
            "pane.send_keys",
            json!({"pane_id": pane_id, "keys": keys}),
            pick_ok,
        )
        .await
    }

    pub async fn pane_split(&self, params: &PaneSplitParams) -> Result<PaneInfo, HerdrError> {
        self.call("pane.split", params, |r| match r {
            ResponseResult::PaneInfo { pane } => Some(pane),
            _ => None,
        })
        .await
    }

    pub async fn pane_close(&self, pane_id: &str) -> Result<(), HerdrError> {
        self.call("pane.close", json!({"pane_id": pane_id}), pick_ok)
            .await
    }

    pub async fn pane_report_metadata(
        &self,
        pane_id: &str,
        source: &str,
        tokens: &[(&str, &str)],
    ) -> Result<(), HerdrError> {
        let tokens: serde_json::Map<String, Value> = tokens
            .iter()
            .map(|(key, value)| ((*key).to_string(), json!(value)))
            .collect();
        self.call(
            "pane.report_metadata",
            json!({"pane_id": pane_id, "source": source, "tokens": tokens}),
            pick_ok,
        )
        .await
    }

    pub async fn tab_create(&self, params: &TabCreateParams) -> Result<TabCreated, HerdrError> {
        self.call("tab.create", params, |r| match r {
            ResponseResult::TabCreated(created) => Some(created),
            _ => None,
        })
        .await
    }

    /// Moves a pane, process and all, into a new tab of `workspace_id`, without focusing it.
    pub async fn pane_move_to_new_tab(
        &self,
        pane_id: &str,
        workspace_id: &str,
        label: Option<&str>,
    ) -> Result<PaneMoveResult, HerdrError> {
        let mut destination = json!({"type": "new_tab", "workspace_id": workspace_id});
        if let Some(label) = label {
            destination["label"] = json!(label);
        }
        self.call(
            "pane.move",
            json!({"pane_id": pane_id, "destination": destination, "focus": false}),
            |r| match r {
                ResponseResult::PaneMove { move_result } => Some(move_result),
                _ => None,
            },
        )
        .await
    }

    pub async fn tab_close(&self, tab_id: &str) -> Result<(), HerdrError> {
        self.call("tab.close", json!({"tab_id": tab_id}), pick_ok)
            .await
    }

    pub async fn workspace_close(&self, workspace_id: &str) -> Result<(), HerdrError> {
        self.call(
            "workspace.close",
            json!({"workspace_id": workspace_id}),
            pick_ok,
        )
        .await
    }

    pub async fn workspace_create(
        &self,
        params: &WorkspaceCreateParams,
    ) -> Result<WorkspaceCreated, HerdrError> {
        self.call("workspace.create", params, |r| match r {
            ResponseResult::WorkspaceCreated(created) => Some(created),
            _ => None,
        })
        .await
    }

    pub async fn worktree_create(
        &self,
        params: &WorktreeCreateParams,
    ) -> Result<WorktreeCreated, HerdrError> {
        self.call("worktree.create", params, |r| match r {
            ResponseResult::WorktreeCreated(created) => Some(created),
            _ => None,
        })
        .await
    }

    pub async fn worktree_list(
        &self,
        params: &WorktreeListParams,
    ) -> Result<WorktreeList, HerdrError> {
        self.call("worktree.list", params, |r| match r {
            ResponseResult::WorktreeList(list) => Some(list),
            _ => None,
        })
        .await
    }

    pub async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<Subscription, HerdrError> {
        let method = "events.subscribe";
        let mut reader = self
            .send(method, json!({"subscriptions": subscriptions}))
            .await?;
        let line = within(
            self.timeout,
            "acknowledge the subscription",
            reader.next_line(),
        )
        .await??
        .ok_or_else(|| {
            HerdrError::protocol("connection closed before the subscription was acknowledged")
        })?;
        match parse_response(method, &line)? {
            Reply::Result(result) if matches!(*result, ResponseResult::SubscriptionStarted {}) => {
                Ok(Subscription::spawn(reader))
            }
            Reply::Result(_) => Err(HerdrError::incompatible(format!(
                "{method}: expected subscription_started"
            ))),
            Reply::Timeout(message) => Err(HerdrError::Api {
                code: ApiErrorCode::Timeout,
                message,
            }),
        }
    }
}

/// Runs `step`, failing with `herdr_timeout` when Herdr takes longer than `limit`.
async fn within<T>(
    limit: Duration,
    what: &str,
    step: impl Future<Output = T>,
) -> Result<T, HerdrError> {
    tokio::time::timeout(limit, step).await.map_err(|_| {
        HerdrError::tool(
            "herdr_timeout",
            format!("herdr did not {what} within {} ms", limit.as_millis()),
        )
    })
}

fn pick_ok(result: ResponseResult) -> Option<()> {
    matches!(result, ResponseResult::Ok {}).then_some(())
}

fn pick_read(result: ResponseResult) -> Option<PaneReadResult> {
    match result {
        ResponseResult::PaneRead { read } => Some(read),
        _ => None,
    }
}

fn pick_result<T>(
    method: &str,
    result: ResponseResult,
    pick: impl FnOnce(ResponseResult) -> Option<T>,
) -> Result<T, HerdrError> {
    pick(result).ok_or_else(|| {
        HerdrError::incompatible(format!("{method}: unexpected result type in response"))
    })
}

fn parse_response(method: &str, line: &str) -> Result<Reply, HerdrError> {
    let mut value: Value = serde_json::from_str(line)
        .map_err(|err| HerdrError::protocol(format!("{method}: invalid json response: {err}")))?;
    if let Some(error) = value.get("error") {
        let (code, message) = error_parts(error);
        return map_error(code, message);
    }
    let result = value.get_mut("result").map(Value::take).ok_or_else(|| {
        HerdrError::protocol(format!("{method}: response has no result or error"))
    })?;
    serde_json::from_value(result)
        .map(|result| Reply::Result(Box::new(result)))
        .map_err(|err| HerdrError::incompatible(format!("{method}: cannot decode result: {err}")))
}

/// The `code` and `message` of a response's `error` object; either is empty when missing.
pub(crate) fn error_parts(error: &Value) -> (&str, String) {
    let field = |name: &str| error.get(name).and_then(Value::as_str).unwrap_or_default();
    (field("code"), field("message").to_string())
}

fn map_error(code: &str, message: String) -> Result<Reply, HerdrError> {
    match code {
        "timeout" => Ok(Reply::Timeout(message)),
        "unknown_method" => Err(HerdrError::incompatible(message)),
        "invalid_request" if message.contains("unknown variant") => {
            Err(HerdrError::incompatible(message))
        }
        _ => Err(HerdrError::Api {
            code: ApiErrorCode::parse(code),
            message,
        }),
    }
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let triple = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(triple)
}
