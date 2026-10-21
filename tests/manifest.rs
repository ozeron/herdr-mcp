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
        entries.len() >= 14,
        "status, install, install-control, remove, tunnel-setup, tunnel-start-full, tunnel-start, tunnel-stop, tunnel-status, tunnel-doctor"
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
    for id in [
        "install",
        "install-control",
        "remove",
        "tunnel-setup",
        "tunnel-start-full",
        "ngrok-start-full",
    ] {
        assert!(ids.contains(&id), "missing pane {id}");
    }
    for pane in &panes {
        assert_eq!(text(pane, "placement"), "split");
    }
    for action in tables(&doc, "actions") {
        let words = bridge_argv(&argv(action));
        let allowed: [&[&str]; 8] = [
            &["install", "--status"],
            &["tunnel", "start"],
            &["tunnel", "stop"],
            &["tunnel", "status"],
            &["tunnel", "doctor"],
            &["tunnel", "ngrok", "start"],
            &["tunnel", "ngrok", "stop"],
            &["tunnel", "ngrok", "status"],
        ];
        assert!(
            allowed.iter().any(|ok| words[1..] == **ok),
            "{words:?} needs no terminal, so it may run as a plain action"
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
            "tunnel-setup" => assert_eq!(words[1..], ["tunnel", "setup"]),
            "tunnel-start-full" => assert_eq!(words[1..], ["tunnel", "start", "--full"]),
            "ngrok-start-full" => {
                assert_eq!(words[1..], ["tunnel", "ngrok", "start", "--full"]);
            }
            other => panic!("unexpected pane {other}"),
        }
    }
}

#[test]
fn tunnel_entries_cover_start_full_start_read_stop_and_status() {
    let doc = manifest();
    let actions = tables(&doc, "actions");
    let panes = tables(&doc, "panes");
    let flags = |id: &str| {
        let entry = actions
            .iter()
            .chain(panes.iter())
            .find(|e| text(e, "id") == id)
            .unwrap_or_else(|| panic!("missing {id}"));
        bridge_argv(&argv(entry))[1..].to_vec()
    };
    assert_eq!(flags("tunnel-start"), ["tunnel", "start"]);
    assert_eq!(flags("tunnel-stop"), ["tunnel", "stop"]);
    assert_eq!(flags("tunnel-status"), ["tunnel", "status"]);
    assert_eq!(flags("tunnel-doctor"), ["tunnel", "doctor"]);
    assert_eq!(flags("tunnel-start-full"), ["tunnel", "start", "--full"]);
    assert_eq!(flags("ngrok-start"), ["tunnel", "ngrok", "start"]);
    assert_eq!(flags("ngrok-stop"), ["tunnel", "ngrok", "stop"]);
    assert_eq!(flags("ngrok-status"), ["tunnel", "ngrok", "status"]);
    assert_eq!(
        flags("ngrok-start-full"),
        ["tunnel", "ngrok", "start", "--full"]
    );
    assert!(
        actions
            .iter()
            .all(|a| !text(a, "id").ends_with("start-full")),
        "the disclosure needs a TTY, so full access runs in a pane"
    );
    assert!(
        !actions
            .iter()
            .chain(panes.iter())
            .any(|e| text(e, "id") == "tunnel"),
        "the foreground run pane is gone"
    );
}

#[test]
fn tunnel_panes_keep_errors_visible_and_full_access_is_labelled() {
    let doc = manifest();
    let panes = tables(&doc, "panes");
    assert_eq!(panes.len(), 6);
    for pane in panes
        .iter()
        .filter(|p| text(p, "id").starts_with("tunnel") || text(p, "id").starts_with("ngrok"))
    {
        assert!(text(pane, "title").starts_with("Herdr MCP: ChatGPT"));
        assert!(
            argv(pane)[2].contains("read _"),
            "press Enter to close wrapper"
        );
    }
    for id in ["tunnel-start-full", "ngrok-start-full"] {
        let full = panes.iter().find(|p| text(p, "id") == id).unwrap();
        assert!(text(full, "title").contains("full access"));
        assert!(text(full, "description").contains("full access"));
    }
    for action in tables(&doc, "actions") {
        if matches!(text(action, "id"), "tunnel-start" | "ngrok-start") {
            assert!(text(action, "description").contains("read tools only"));
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
fn print_config_gives_desktop_apps_the_default_wait_cap() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_str().unwrap();
    let out = print_config(home.path(), &[]);
    let heading = out
        .lines()
        .position(|line| line.starts_with("# Claude Desktop"))
        .expect("a Claude Desktop section");
    let entry = out
        .lines()
        .skip(heading)
        .find(|line| line.starts_with('{'))
        .expect("a Claude Desktop JSON entry");
    let entry: serde_json::Value = serde_json::from_str(entry).expect("valid JSON");
    let server = &entry["mcpServers"]["herdr"];
    assert_eq!(
        server["command"],
        format!("{home_path}/.local/bin/herdr-mcp"),
        "{out}"
    );
    assert_eq!(
        server["args"],
        serde_json::json!(["--tools", "read"]),
        "no --client, so the wait cap stays under the app's tool timeout: {out}"
    );
}

#[test]
fn print_config_prints_control_only_when_asked() {
    let home = tempfile::tempdir().unwrap();
    let out = print_config(home.path(), &["--tools", "control"]);
    assert!(out.contains("--tools control"), "{out}");
    assert!(!out.contains("--tools read"), "{out}");
}
