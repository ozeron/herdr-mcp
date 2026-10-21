//! The Herdr session every tool call talks to. It is process state: the bridge starts in its home
//! session and `herdr_use_session` can move it.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::UnixStream;

use crate::herdr::{DEFAULT_SESSION_NAME, HerdrError, SessionEnv, session_socket};

const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const CUSTOM_NAME: &str = "custom";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSession {
    pub name: String,
    pub socket_path: PathBuf,
    /// The session the bridge started in. Only there does the caller pane mean anything.
    pub home: bool,
}

pub struct Sessions {
    env: SessionEnv,
    home: ActiveSession,
    active: RwLock<ActiveSession>,
}

impl Sessions {
    pub fn new(home_socket: PathBuf, env: SessionEnv) -> Self {
        let home = ActiveSession {
            name: name_of(&home_socket, &env),
            socket_path: home_socket,
            home: true,
        };
        Self {
            env,
            active: RwLock::new(home.clone()),
            home,
        }
    }

    pub fn with_env(self, env: SessionEnv) -> Self {
        Self::new(self.home.socket_path, env)
    }

    pub fn active(&self) -> ActiveSession {
        self.active.read().expect("session lock").clone()
    }

    /// Every known session with whether its socket accepts a connection: default first, then by name.
    pub async fn list(&self) -> Vec<Value> {
        let config_dir = self.env.config_dir();
        let mut found = vec![(
            DEFAULT_SESSION_NAME.to_string(),
            config_dir.join("herdr.sock"),
        )];
        if let Ok(entries) = std::fs::read_dir(config_dir.join("sessions")) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let Ok(socket) = session_socket(&name, &self.env) else {
                    continue;
                };
                if socket.exists() {
                    found.push((name, socket));
                }
            }
        }
        if !found.iter().any(|(name, _)| *name == self.home.name) {
            found.push((self.home.name.clone(), self.home.socket_path.clone()));
        }
        found.sort_by(|a, b| {
            let rank = |name: &str| name != DEFAULT_SESSION_NAME;
            (rank(&a.0), &a.0).cmp(&(rank(&b.0), &b.0))
        });
        let active = self.active();
        let running =
            futures_util::future::join_all(found.iter().map(|(_, s)| is_running(s))).await;
        found
            .into_iter()
            .zip(running)
            .map(|((name, socket), running)| {
                json!({
                    "name": name,
                    "socket": socket,
                    "running": running,
                    "active": name == active.name,
                })
            })
            .collect()
    }

    /// Makes `name` the active session and returns the previous one.
    pub async fn switch(&self, name: &str) -> Result<(ActiveSession, ActiveSession), HerdrError> {
        let socket = if name == self.home.name {
            self.home.socket_path.clone()
        } else {
            let socket = session_socket(name, &self.env)
                .map_err(|err| HerdrError::invalid_params(err.to_string()))?;
            if name != DEFAULT_SESSION_NAME && !socket.exists() {
                return Err(HerdrError::tool(
                    "session_not_found",
                    format!("no herdr session named {name:?}; call herdr_sessions to list them"),
                ));
            }
            socket
        };
        if !is_running(&socket).await {
            return Err(HerdrError::tool(
                "session_not_running",
                format!("herdr session {name:?} is not running"),
            ));
        }
        let next = ActiveSession {
            name: name.into(),
            home: socket == self.home.socket_path,
            socket_path: socket,
        };
        let previous = std::mem::replace(
            &mut *self.active.write().expect("session lock"),
            next.clone(),
        );
        Ok((previous, next))
    }
}

async fn is_running(socket: &Path) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(socket)).await,
        Ok(Ok(_))
    )
}

fn name_of(socket: &Path, env: &SessionEnv) -> String {
    let config_dir = env.config_dir();
    if socket == config_dir.join("herdr.sock") {
        return DEFAULT_SESSION_NAME.into();
    }
    socket
        .strip_prefix(config_dir.join("sessions"))
        .ok()
        .and_then(|rest| {
            let mut parts = rest.iter();
            let name = parts.next()?.to_str()?;
            (parts.next()? == "herdr.sock" && parts.next().is_none()).then(|| name.to_string())
        })
        .unwrap_or_else(|| CUSTOM_NAME.into())
}
