use std::path::PathBuf;
use std::process::Command;

use clap::Parser;
use herdr_mcp::cli::Args;
use toml_edit::{DocumentMut, Table};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn manifest() -> DocumentMut {
    let text = std::fs::read_to_string(root().join("herdr-plugin.toml"))
        .expect("herdr-plugin.toml exists at the repo root");
    text.parse().expect("manifest parses as TOML")
}

fn tables<'a>(doc: &'a DocumentMut, name: &str) -> Vec<&'a Table> {
    doc.get(name)
        .and_then(|item| item.as_array_of_tables())
        .map(|array| array.iter().collect())
        .unwrap_or_default()
}

fn argv(table: &Table) -> Vec<String> {
    table["command"]
        .as_array()
        .expect("command is an argv array")
        .iter()
        .map(|part| part.as_str().expect("argv parts are strings").to_owned())
        .collect()
}

fn text<'a>(table: &'a Table, key: &str) -> &'a str {
    table[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing string field {key}"))
}

/// The bridge argv of an entry: the command itself, or the first statement of a `sh -c` wrapper.
fn bridge_argv(command: &[String]) -> Vec<String> {
    let line = match command {
        [sh, flag, script] if sh == "sh" && flag == "-c" => script
            .split(';')
            .next()
            .expect("split yields one part")
            .to_owned(),
        _ => command.join(" "),
    };
    line.split_whitespace().map(str::to_owned).collect()
}

#[test]
fn manifest_has_required_fields() {
    let doc = manifest();
    for key in ["id", "name", "version", "min_herdr_version"] {
        assert!(
            !doc[key].as_str().unwrap_or_default().is_empty(),
            "{key} is required"
        );
    }
    let (major, minor, patch) = herdr_mcp::herdr::MIN_HERDR_VERSION;
    assert_eq!(
        doc["min_herdr_version"].as_str(),
        Some(format!("{major}.{minor}.{patch}").as_str())
    );
    assert_eq!(doc["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
}

#[test]
fn build_installs_the_binary_under_the_plugin_root() {
    let doc = manifest();
    let builds = tables(&doc, "build");
    assert_eq!(builds.len(), 2);
    assert_eq!(
        argv(builds[0]),
        ["cargo", "install", "--path", ".", "--root", ".", "--locked"]
    );
}

#[test]
fn second_build_step_relinks_the_shim_to_the_new_build() {
    let doc = manifest();
    let builds = tables(&doc, "build");
    let words = argv(builds[1]);
    assert_eq!(words, ["bin/herdr-mcp", "install", "--relink-shim"]);
    let mut full = vec!["herdr-mcp".to_owned()];
    full.extend(words[1..].iter().cloned());
    Args::try_parse_from(&full).unwrap_or_else(|err| panic!("binary rejects {full:?}: {err}"));
}

#[test]
fn every_command_runs_bin_herdr_mcp_with_a_subcommand_it_accepts() {
    let doc = manifest();
    let entries: Vec<&Table> = tables(&doc, "actions")
        .into_iter()
        .chain(tables(&doc, "panes"))
        .collect();
    assert!(
        entries.len() >= 4,
        "status, install, install-control, remove"
    );
    for entry in entries {
        let id = text(entry, "id");
        let words = bridge_argv(&argv(entry));
        assert_eq!(words[0], "bin/herdr-mcp", "{id} must run bin/herdr-mcp");
        let mut full = vec!["herdr-mcp".to_owned()];
        full.extend(words[1..].iter().cloned());
        let parsed = Args::try_parse_from(&full)
            .unwrap_or_else(|err| panic!("{id}: binary rejects {full:?}: {err}"));
        assert!(parsed.command.is_some(), "{id} must name a subcommand");
    }
}

#[test]
fn install_and_remove_open_a_pane_because_actions_have_no_tty() {
    let doc = manifest();
    let panes = tables(&doc, "panes");
    let ids: Vec<&str> = panes.iter().map(|pane| text(pane, "id")).collect();
    for id in ["install", "install-control", "remove"] {
        assert!(ids.contains(&id), "missing pane {id}");
    }
    for pane in &panes {
        assert_eq!(text(pane, "placement"), "split");
    }
    for action in tables(&doc, "actions") {
        let words = bridge_argv(&argv(action));
        assert_eq!(
            words,
            ["bin/herdr-mcp", "install", "--status"],
            "only status runs as a plain action"
        );
    }
}

#[test]
fn install_panes_pass_the_right_flags() {
    let doc = manifest();
    for pane in tables(&doc, "panes") {
        let words = bridge_argv(&argv(pane));
        match text(pane, "id") {
            "install" => assert_eq!(words[1..], ["install"]),
            "install-control" => assert_eq!(words[1..], ["install", "--control"]),
            "remove" => assert_eq!(words[1..], ["install", "--remove"]),
            other => panic!("unexpected pane {other}"),
        }
    }
}

fn print_config(home: &std::path::Path, extra: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-mcp"))
        .arg("print-config")
        .args(extra)
        .env_clear()
        .env("HOME", home)
        .output()
        .expect("run print-config");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("utf-8 output")
}

#[test]
fn print_config_defaults_to_read_and_names_only_the_shim() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_str().unwrap();
    let out = print_config(home.path(), &[]);
    let shim = format!("{home_path}/.local/bin/herdr-mcp");
    assert!(out.contains(&shim), "{out}");
    assert!(out.contains("--tools read"), "{out}");
    assert!(!out.contains("control"), "{out}");
    assert!(out.contains("claude mcp add"), "{out}");
    assert!(out.contains("[mcp_servers.herdr]"), "{out}");
    let env_vars = herdr_mcp::install::codex::ENV_VARS
        .map(|name| format!("\"{name}\""))
        .join(", ");
    assert!(
        out.contains(&format!("env_vars = [{env_vars}]")),
        "the Codex TOML block lists env_vars: {out}"
    );
    let add_line = out
        .lines()
        .position(|line| line.starts_with("codex mcp add"))
        .expect("codex mcp add line");
    let note = out.lines().nth(add_line + 1).unwrap_or_default();
    assert!(
        note.starts_with('#') && note.contains("env_vars") && note.contains("tool_timeout_sec"),
        "the comment after `codex mcp add` names env_vars and tool_timeout_sec: {out}"
    );
    assert!(note.contains("herdr-mcp install"), "{out}");
    assert!(
        !out.replace(&shim, "").contains(home_path),
        "a home path other than the shim leaked: {out}"
    );
}

#[test]
fn print_config_prints_control_only_when_asked() {
    let home = tempfile::tempdir().unwrap();
    let out = print_config(home.path(), &["--tools", "control"]);
    assert!(out.contains("--tools control"), "{out}");
    assert!(!out.contains("--tools read"), "{out}");
}
