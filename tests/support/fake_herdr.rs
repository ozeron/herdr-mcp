#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub enum StreamEnd {
    Lost,
    Close,
    Hold,
}

pub enum Script {
    Reply(Value),
    Error { code: String, message: String },
    Raw(String),
    Hold,
    Drop,
    Stream { events: Vec<Value>, end: StreamEnd },
}

impl Script {
    pub fn error(code: &str, message: &str) -> Self {
        Script::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Default)]
struct State {
    scripts: HashMap<String, VecDeque<Script>>,
    requests: Vec<Value>,
    disconnects: usize,
}

pub struct FakeHerdr {
    _dir: TempDir,
    path: PathBuf,
    state: Arc<Mutex<State>>,
    disconnected: Arc<Notify>,
    task: JoinHandle<()>,
}

impl FakeHerdr {
    pub async fn start() -> Self {
        let dir = tempfile::Builder::new().prefix("hm").tempdir().unwrap();
        let path = dir.path().join("h.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let disconnected = Arc::new(Notify::new());
        let task = tokio::spawn(accept_loop(
            listener,
            Arc::clone(&state),
            Arc::clone(&disconnected),
        ));
        Self {
            _dir: dir,
            path,
            state,
            disconnected,
            task,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn on(&self, method: &str, script: Script) {
        self.state
            .lock()
            .unwrap()
            .scripts
            .entry(method.into())
            .or_default()
            .push_back(script);
    }

    pub fn reply(&self, method: &str, result: Value) {
        self.on(method, Script::Reply(result));
    }

    pub fn requests(&self) -> Vec<Value> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn last_request(&self) -> Value {
        self.requests().pop().expect("no request recorded")
    }

    pub async fn wait_disconnect(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.disconnected.notified();
                if self.state.lock().unwrap().disconnects > 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("fake never saw a disconnect");
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn accept_loop(listener: UnixListener, state: Arc<Mutex<State>>, notify: Arc<Notify>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(handle(stream, Arc::clone(&state), Arc::clone(&notify)));
    }
}

async fn handle(stream: UnixStream, state: Arc<Mutex<State>>, notify: Arc<Notify>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let request: Value = serde_json::from_str(line.trim()).expect("client sent invalid json");
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default().to_string();
    let script = {
        let mut state = state.lock().unwrap();
        state.requests.push(request);
        state
            .scripts
            .get_mut(&method)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| Script::error("no_script", &method))
    };

    match script {
        Script::Reply(result) => {
            write_line(&mut write, &json!({"id": id, "result": result})).await;
        }
        Script::Error { code, message } => {
            write_line(
                &mut write,
                &json!({"id": id, "error": {"code": code, "message": message}}),
            )
            .await;
        }
        Script::Raw(raw) => {
            let _ = write.write_all(raw.as_bytes()).await;
            let _ = write.write_all(b"\n").await;
        }
        Script::Drop => {}
        Script::Hold => wait_for_eof(&mut reader, &state, &notify).await,
        Script::Stream { events, end } => {
            write_line(
                &mut write,
                &json!({"id": id, "result": {"type": "subscription_started"}}),
            )
            .await;
            for event in events {
                write_line(&mut write, &event).await;
            }
            match end {
                StreamEnd::Lost => {
                    write_line(
                        &mut write,
                        &json!({"id": id, "error": {"code": "events_lost", "message": "fell behind"}}),
                    )
                    .await;
                }
                StreamEnd::Close => {}
                StreamEnd::Hold => {
                    wait_for_eof(&mut reader, &state, &notify).await;
                }
            }
        }
    }
}

async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    let _ = write.write_all(&bytes).await;
}

async fn wait_for_eof(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    state: &Mutex<State>,
    notify: &Notify,
) {
    let mut sink = [0u8; 64];
    while matches!(reader.read(&mut sink).await, Ok(n) if n > 0) {}
    state.lock().unwrap().disconnects += 1;
    notify.notify_waiters();
}
