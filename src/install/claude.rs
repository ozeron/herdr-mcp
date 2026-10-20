use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::Value;

use super::status::Entry;
use super::{InstallEnv, InstallError, run_cli};
use crate::cli::ToolMode;
use crate::providers::server_args;

pub struct Read {
    pub entry: Option<Entry>,
    pub shadows: Vec<String>,
}

pub fn config_path(env: &InstallEnv) -> PathBuf {
    match &env.claude_config_dir {
        Some(dir) => dir.join(".claude.json"),
        None => env.home.join(".claude.json"),
    }
}

fn entry_from(value: &Value) -> Option<Entry> {
    let server = value.get("mcpServers")?.get("herdr")?;
    Some(Entry {
        command: server.get("command")?.as_str()?.to_owned(),
        args: server
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(|arg| arg.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        env_vars: None,
        tool_timeout_sec: None,
    })
}

pub fn read(env: &InstallEnv) -> Result<Read, InstallError> {
    let path = config_path(env);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Read {
                entry: None,
                shadows: Vec::new(),
            });
        }
        Err(err) => return Err(InstallError::new(format!("{}: {err}", path.display()))),
    };
    let json: Value = serde_json::from_str(&text)
        .map_err(|err| InstallError::new(format!("{}: {err}", path.display())))?;
    let shadows = json
        .get("projects")
        .and_then(Value::as_object)
        .map(|projects| {
            projects
                .iter()
                .filter(|(_, project)| {
                    project
                        .get("mcpServers")
                        .is_some_and(|servers| servers.get("herdr").is_some())
                })
                .map(|(dir, _)| dir.clone())
                .collect()
        })
        .unwrap_or_default();
    Ok(Read {
        entry: entry_from(&json),
        shadows,
    })
}

pub fn add(env: &InstallEnv, mode: ToolMode) -> Result<(), InstallError> {
    let shim = env.shim_path().display().to_string();
    let mut args = vec!["mcp", "add", "--scope", "user", "herdr", "--", &shim];
    args.extend(server_args("claude", mode));
    run_cli(env, "claude", &args)
}

pub fn remove(env: &InstallEnv) -> Result<(), InstallError> {
    run_cli(env, "claude", &["mcp", "remove", "herdr", "-s", "user"])
}
