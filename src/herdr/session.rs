use std::path::PathBuf;

use thiserror::Error;

pub const SOCKET_PATH_ENV: &str = "HERDR_SOCKET_PATH";
pub const SESSION_ENV: &str = "HERDR_SESSION";
pub const DEFAULT_SESSION_NAME: &str = "default";

const MAX_SESSION_NAME_LEN: usize = 64;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid herdr session name {name:?}: {reason}")]
pub struct SessionError {
    pub name: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEnv {
    pub socket_path: Option<PathBuf>,
    pub session: Option<String>,
    pub xdg_config_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl SessionEnv {
    pub fn from_process() -> Self {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|value| !value.is_empty())
        }
        Self {
            socket_path: var(SOCKET_PATH_ENV).map(PathBuf::from),
            session: var(SESSION_ENV),
            xdg_config_home: var("XDG_CONFIG_HOME").map(PathBuf::from),
            home: var("HOME").map(PathBuf::from),
        }
    }

    fn config_dir(&self) -> PathBuf {
        if let Some(xdg) = &self.xdg_config_home {
            return xdg.join("herdr");
        }
        match &self.home {
            Some(home) => home.join(".config/herdr"),
            None => std::env::temp_dir().join("herdr"),
        }
    }
}

/// Mirrors Herdr's own resolution: `--session` > `HERDR_SOCKET_PATH` > `HERDR_SESSION` > default.
pub fn resolve_socket_path(
    session_arg: Option<&str>,
    env: &SessionEnv,
) -> Result<PathBuf, SessionError> {
    if let Some(name) = session_arg {
        return session_socket(name, env);
    }
    if let Some(path) = &env.socket_path {
        return Ok(path.clone());
    }
    if let Some(name) = &env.session {
        return session_socket(name, env);
    }
    Ok(env.config_dir().join("herdr.sock"))
}

fn session_socket(name: &str, env: &SessionEnv) -> Result<PathBuf, SessionError> {
    if name == DEFAULT_SESSION_NAME {
        return Ok(env.config_dir().join("herdr.sock"));
    }
    validate_name(name)?;
    Ok(env
        .config_dir()
        .join("sessions")
        .join(name)
        .join("herdr.sock"))
}

fn validate_name(name: &str) -> Result<(), SessionError> {
    let reason = if name.is_empty() {
        Some("cannot be empty")
    } else if name.len() > MAX_SESSION_NAME_LEN {
        Some("cannot be longer than 64 bytes")
    } else if name == "." || name == ".." {
        Some("cannot be . or ..")
    } else if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        Some("may only contain ASCII letters, numbers, '.', '_' and '-'")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(SessionError {
            name: name.into(),
            reason,
        }),
        None => Ok(()),
    }
}
