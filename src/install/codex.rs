use std::fs;
use std::io;
use std::path::PathBuf;

use toml_edit::{Array, DocumentMut, Item, Value};

use super::status::Entry;
use super::{InstallEnv, InstallError, run_cli};
use crate::cli::ToolMode;
use crate::providers::server_args;

pub const ENV_VARS: [&str; 7] = [
    "HERDR_SOCKET_PATH",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_ENV",
    "HERDR_SESSION",
    "XDG_CONFIG_HOME",
];
pub const TOOL_TIMEOUT_SEC: i64 = 330;
const MARKER: &str = " # herdr-mcp";

pub fn config_path(env: &InstallEnv) -> PathBuf {
    match &env.codex_home {
        Some(dir) => dir.join("config.toml"),
        None => env.home.join(".codex/config.toml"),
    }
}

fn load(env: &InstallEnv) -> Result<Option<DocumentMut>, InstallError> {
    let path = config_path(env);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(InstallError::new(format!("{}: {err}", path.display()))),
    };
    text.parse::<DocumentMut>()
        .map(Some)
        .map_err(|err| InstallError::new(format!("{}: {err}", path.display())))
}

fn strings(item: &Item) -> Option<Vec<String>> {
    item.as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

pub fn read(env: &InstallEnv) -> Result<Option<Entry>, InstallError> {
    let Some(doc) = load(env)? else {
        return Ok(None);
    };
    let Some(table) = doc
        .get("mcp_servers")
        .and_then(|servers| servers.get("herdr"))
        .and_then(Item::as_table_like)
    else {
        return Ok(None);
    };
    Ok(Some(Entry {
        command: table
            .get("command")
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_owned(),
        args: table.get("args").and_then(strings).unwrap_or_default(),
        env_vars: table.get("env_vars").and_then(strings),
        tool_timeout_sec: table.get("tool_timeout_sec").and_then(Item::as_integer),
    }))
}

fn marked(mut value: Value) -> Item {
    value.decor_mut().set_suffix(MARKER);
    Item::Value(value)
}

fn write_extras(env: &InstallEnv) -> Result<(), InstallError> {
    let path = config_path(env);
    let mut doc = load(env)?
        .ok_or_else(|| InstallError::new(format!("{} was not written", path.display())))?;
    let table = doc
        .get_mut("mcp_servers")
        .and_then(|servers| servers.get_mut("herdr"))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| InstallError::new("codex wrote no [mcp_servers.herdr] table"))?;
    let mut vars = Array::new();
    for name in ENV_VARS {
        vars.push(name);
    }
    table.insert("env_vars", marked(Value::Array(vars)));
    table.insert("tool_timeout_sec", marked(Value::from(TOOL_TIMEOUT_SEC)));

    let tmp = path.with_extension("toml.herdr-mcp.tmp");
    let fail = |err: io::Error| InstallError::new(format!("{}: {err}", path.display()));
    fs::write(&tmp, doc.to_string()).map_err(fail)?;
    if let Ok(meta) = fs::metadata(&path) {
        fs::set_permissions(&tmp, meta.permissions()).map_err(fail)?;
    }
    fs::rename(&tmp, &path).map_err(fail)
}

pub fn add(env: &InstallEnv, mode: ToolMode) -> Result<(), InstallError> {
    let shim = env.shim_path().display().to_string();
    let mut args = vec!["mcp", "add", "herdr", "--", &shim];
    args.extend(server_args("codex", mode));
    run_cli(env, "codex", &args)?;
    write_extras(env)
}

pub fn remove(env: &InstallEnv) -> Result<(), InstallError> {
    run_cli(env, "codex", &["mcp", "remove", "herdr"])
}
