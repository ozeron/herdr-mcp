use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use herdr_mcp::cli::ToolMode;
use herdr_mcp::install::status::{McpStatus, State, classify, mcp_status};
use herdr_mcp::install::{Action, InstallEnv, Io, Options, run};
use herdr_mcp::profile::ClientKind;
use tempfile::TempDir;

const FAKE_CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/fake_cli");

struct Sandbox {
    _tmp: TempDir,
    root: PathBuf,
    env: InstallEnv,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        let exe = root.join("bin/herdr-mcp");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let env = InstallEnv {
            home: root.join("home"),
            claude_config_dir: None,
            codex_home: None,
            path: OsString::from(format!("{FAKE_CLI}:/usr/bin:/bin")),
            exe,
        };
        Self {
            _tmp: tmp,
            root,
            env,
        }
    }

    fn home(&self) -> &Path {
        &self.env.home
    }

    fn shim(&self) -> PathBuf {
        self.home().join(".local/bin/herdr-mcp")
    }

    fn claude_json(&self) -> PathBuf {
        self.home().join(".claude.json")
    }

    fn codex_toml(&self) -> PathBuf {
        self.home().join(".codex/config.toml")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.home().join(".fake_cli.log")).unwrap_or_default()
    }

    fn fake(&self, program: &str, args: &[&str]) {
        let status = Command::new(Path::new(FAKE_CLI).join(program))
            .args(args)
            .env("HOME", self.home())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .status()
            .unwrap();
        assert!(status.success());
    }
}

struct Outcome {
    code: i32,
    out: String,
    err: String,
}

fn opts(action: Action, control: bool, yes: bool) -> Options {
    Options {
        control,
        action,
        yes,
    }
}

fn run_with(env: &InstallEnv, options: &Options, tty: bool, input: &str) -> Outcome {
    let mut stdin = Cursor::new(input.as_bytes().to_vec());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(
        options,
        env,
        &mut Io {
            stdin_is_tty: tty,
            input: &mut stdin,
            out: &mut out,
            err: &mut err,
        },
    );
    Outcome {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn install(sb: &Sandbox, control: bool) -> Outcome {
    run_with(&sb.env, &opts(Action::Install, control, true), false, "")
}

fn status(sb: &Sandbox) -> Outcome {
    run_with(&sb.env, &opts(Action::Status, false, false), false, "")
}

fn remove(sb: &Sandbox) -> Outcome {
    run_with(&sb.env, &opts(Action::Remove, false, true), false, "")
}

fn installed() -> Sandbox {
    let sb = Sandbox::new();
    let outcome = install(&sb, false);
    assert_eq!(outcome.code, 0, "{}{}", outcome.out, outcome.err);
    sb
}

#[test]
fn absent_install_with_yes_writes_read_mode_entries_and_shim() {
    let sb = Sandbox::new();
    let outcome = install(&sb, false);
    assert_eq!(outcome.code, 0, "{}{}", outcome.out, outcome.err);

    let shim = sb.shim();
    let shim = shim.display();
    let log = sb.log();
    assert!(
        log.contains(&format!(
            "claude mcp add --scope user herdr -- {shim} --client claude --tools read"
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "codex mcp add herdr -- {shim} --client codex --tools read"
        )),
        "{log}"
    );

    let toml = fs::read_to_string(sb.codex_toml()).unwrap();
    let env_line = toml.lines().find(|l| l.starts_with("env_vars")).unwrap();
    for name in [
        "HERDR_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_ENV",
        "HERDR_SESSION",
        "XDG_CONFIG_HOME",
    ] {
        assert!(env_line.contains(&format!("\"{name}\"")), "{env_line}");
    }
    assert!(env_line.ends_with("# herdr-mcp"), "{env_line}");
    let timeout_line = toml
        .lines()
        .find(|l| l.starts_with("tool_timeout_sec"))
        .unwrap();
    assert!(timeout_line.contains("330"), "{timeout_line}");
    assert!(timeout_line.ends_with("# herdr-mcp"), "{timeout_line}");

    assert_eq!(fs::read_link(sb.shim()).unwrap(), sb.env.exe);

    let report = status(&sb);
    assert!(
        report.out.contains("claude: ours (mode: read)"),
        "{}",
        report.out
    );
    assert!(
        report.out.contains("codex: ours (mode: read)"),
        "{}",
        report.out
    );
}

#[test]
fn missing_local_bin_is_created() {
    let sb = Sandbox::new();
    assert!(!sb.home().join(".local").exists());
    assert_eq!(install(&sb, false).code, 0);
    assert!(sb.shim().is_symlink());
    assert!(sb.shim().exists());
}

#[test]
fn control_install_upgrades_an_ours_read_entry() {
    let sb = installed();
    let outcome = install(&sb, true);
    assert_eq!(outcome.code, 0, "{}{}", outcome.out, outcome.err);

    let log = sb.log();
    assert!(log.contains("claude mcp remove herdr -s user"), "{log}");
    assert!(log.contains("--client claude --tools control"), "{log}");
    assert!(log.contains("--client codex --tools control"), "{log}");
    assert!(
        fs::read_to_string(sb.codex_toml())
            .unwrap()
            .contains("tool_timeout_sec")
    );

    let report = status(&sb);
    assert!(
        report.out.contains("claude: ours (mode: control)"),
        "{}",
        report.out
    );
    assert!(
        report.out.contains("codex: ours (mode: control)"),
        "{}",
        report.out
    );
}

#[test]
fn plan_states_the_three_exposures_and_control_adds_shell_authority() {
    let sb = Sandbox::new();
    let read = run_with(&sb.env, &opts(Action::Install, false, false), false, "");
    for text in [
        "Pane output reaches your model provider",
        "default Herdr session",
    ] {
        assert!(read.out.contains(text), "{}", read.out);
    }
    assert!(read.out.contains("Pass --control"), "{}", read.out);
    assert!(!read.out.contains("shell authority"), "{}", read.out);

    let control = run_with(&sb.env, &opts(Action::Install, true, false), false, "");
    for text in [
        "Pane output reaches your model provider",
        "default Herdr session",
        "shell authority",
    ] {
        assert!(control.out.contains(text), "{}", control.out);
    }
}

#[test]
fn rerunning_codex_add_drops_extras_so_status_reports_drifted_and_install_repairs() {
    let sb = installed();
    let shim = sb.shim();
    sb.fake(
        "codex",
        &[
            "mcp",
            "add",
            "herdr",
            "--",
            shim.to_str().unwrap(),
            "--client",
            "codex",
            "--tools",
            "read",
        ],
    );
    assert!(
        !fs::read_to_string(sb.codex_toml())
            .unwrap()
            .contains("env_vars")
    );

    let report = status(&sb);
    assert!(report.out.contains("codex: drifted"), "{}", report.out);
    assert!(report.out.contains("env_vars"), "{}", report.out);
    assert!(report.out.contains("tool_timeout_sec"), "{}", report.out);
    assert!(report.out.contains("claude: ours"), "{}", report.out);

    assert_eq!(install(&sb, false).code, 0);
    assert!(status(&sb).out.contains("codex: ours (mode: read)"));
    assert!(
        fs::read_to_string(sb.codex_toml())
            .unwrap()
            .contains("env_vars")
    );
}

#[test]
fn foreign_entry_makes_install_refuse_and_change_nothing() {
    let sb = Sandbox::new();
    let original = r#"{"mcpServers":{"herdr":{"type":"stdio","command":"/usr/bin/other","args":[],"env":{}}}}"#;
    fs::write(sb.claude_json(), original).unwrap();

    let outcome = install(&sb, false);
    assert_ne!(outcome.code, 0);
    assert!(outcome.err.contains("foreign"), "{}", outcome.err);
    assert_eq!(fs::read_to_string(sb.claude_json()).unwrap(), original);
    assert!(!sb.log().contains("mcp add"), "{}", sb.log());
    assert!(!sb.shim().exists() && !sb.shim().is_symlink());
    assert!(!sb.codex_toml().exists());
    assert!(status(&sb).out.contains("claude: foreign"));
}

#[test]
fn foreign_file_at_the_shim_path_is_refused() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.shim().parent().unwrap()).unwrap();
    fs::write(sb.shim(), "not ours").unwrap();

    let outcome = install(&sb, false);
    assert_ne!(outcome.code, 0);
    assert!(
        outcome.err.contains(&sb.shim().display().to_string()),
        "{}",
        outcome.err
    );
    assert_eq!(fs::read_to_string(sb.shim()).unwrap(), "not ours");
    assert!(!sb.log().contains("mcp add"), "{}", sb.log());
}

#[test]
fn dangling_shim_is_shim_broken_and_install_relinks() {
    let sb = installed();
    fs::remove_file(sb.shim()).unwrap();
    symlink(sb.root.join("gone/herdr-mcp"), sb.shim()).unwrap();

    let report = status(&sb);
    assert!(report.out.contains("claude: shim_broken"), "{}", report.out);
    assert!(report.out.contains("codex: shim_broken"), "{}", report.out);

    assert_eq!(install(&sb, false).code, 0);
    assert_eq!(fs::read_link(sb.shim()).unwrap(), sb.env.exe);
    assert!(status(&sb).out.contains("claude: ours"));
}

#[test]
fn shim_pointing_at_a_moved_binary_is_drifted_and_install_relinks() {
    let sb = installed();
    let moved = sb.root.join("moved/herdr-mcp");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::write(&moved, "").unwrap();
    fs::remove_file(sb.shim()).unwrap();
    symlink(&moved, sb.shim()).unwrap();

    let report = status(&sb);
    assert!(report.out.contains("claude: drifted"), "{}", report.out);
    assert!(report.out.contains("shim"), "{}", report.out);

    assert_eq!(install(&sb, false).code, 0);
    assert_eq!(fs::read_link(sb.shim()).unwrap(), sb.env.exe);
}

#[test]
fn remove_deletes_only_entries_that_point_at_the_shim() {
    let sb = installed();
    let foreign = "[mcp_servers.herdr]\ncommand = \"/usr/bin/other\"\nargs = []\n";
    fs::write(sb.codex_toml(), foreign).unwrap();

    let outcome = remove(&sb);
    assert_eq!(outcome.code, 0, "{}{}", outcome.out, outcome.err);
    assert!(
        !fs::read_to_string(sb.claude_json())
            .unwrap()
            .contains("herdr-mcp")
    );
    assert!(!sb.log().contains("codex mcp remove"), "{}", sb.log());
    assert_eq!(fs::read_to_string(sb.codex_toml()).unwrap(), foreign);
    assert!(outcome.out.contains("foreign"), "{}", outcome.out);
}

#[test]
fn remove_deletes_both_entries_and_the_shim() {
    let sb = installed();
    assert_eq!(remove(&sb).code, 0);
    assert!(sb.log().contains("codex mcp remove herdr"));
    assert!(
        !fs::read_to_string(sb.codex_toml())
            .unwrap()
            .contains("mcp_servers.herdr")
    );
    assert!(!sb.shim().is_symlink());
    let report = status(&sb);
    assert!(report.out.contains("claude: absent"), "{}", report.out);
    assert!(report.out.contains("codex: absent"), "{}", report.out);
}

#[test]
fn tty_without_yes_prompts_and_writes_only_on_confirmation() {
    let sb = Sandbox::new();
    let options = opts(Action::Install, false, false);

    let declined = run_with(&sb.env, &options, true, "n\n");
    assert_ne!(declined.code, 0);
    assert!(declined.out.contains("[y/N]"), "{}", declined.out);
    assert!(!sb.log().contains("mcp add"));
    assert!(!sb.shim().is_symlink());

    let accepted = run_with(&sb.env, &options, true, "y\n");
    assert_eq!(accepted.code, 0, "{}{}", accepted.out, accepted.err);
    assert!(sb.log().contains("claude mcp add"));
    assert!(sb.shim().is_symlink());
}

#[test]
fn no_tty_without_yes_prints_the_plan_and_exits_nonzero_without_writing() {
    let sb = Sandbox::new();
    let outcome = run_with(&sb.env, &opts(Action::Install, false, false), false, "y\n");
    assert_ne!(outcome.code, 0);
    assert!(
        outcome
            .out
            .contains("Pane output reaches your model provider")
    );
    assert!(outcome.err.contains("--yes"), "{}", outcome.err);
    assert!(!sb.log().contains("mcp add"));
    assert!(!sb.shim().is_symlink());
    assert!(!sb.codex_toml().exists());
}

#[test]
fn binary_install_without_tty_or_yes_exits_nonzero_without_writing() {
    let sb = Sandbox::new();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-mcp"))
        .arg("install")
        .env("HOME", sb.home())
        .env("PATH", &sb.env.path)
        .env("CLAUDE_CONFIG_DIR", sb.root.join("claude-cfg"))
        .env("CODEX_HOME", sb.root.join("codex-home"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Pane output reaches your model provider"),
        "{stdout}"
    );
    assert!(!sb.log().contains("mcp add"));
    assert!(!sb.shim().is_symlink());
}

#[test]
fn config_dir_overrides_are_used_and_printed() {
    let mut sb = Sandbox::new();
    let claude_dir = sb.root.join("claude-cfg");
    let codex_dir = sb.root.join("codex-home");
    sb.env.claude_config_dir = Some(claude_dir.clone());
    sb.env.codex_home = Some(codex_dir.clone());

    let plan = run_with(&sb.env, &opts(Action::Install, false, false), false, "");
    assert!(
        plan.out
            .contains(&claude_dir.join(".claude.json").display().to_string())
    );
    assert!(
        plan.out
            .contains(&codex_dir.join("config.toml").display().to_string())
    );

    assert_eq!(install(&sb, false).code, 0);
    assert!(claude_dir.join(".claude.json").exists());
    assert!(codex_dir.join("config.toml").exists());
    assert!(!sb.claude_json().exists());
    assert!(!sb.codex_toml().exists());

    let report = status(&sb);
    assert!(
        report
            .out
            .contains(&claude_dir.join(".claude.json").display().to_string())
    );
    assert!(
        report
            .out
            .contains(&codex_dir.join("config.toml").display().to_string())
    );
    assert!(report.out.contains("claude: ours"), "{}", report.out);
}

#[test]
fn project_scope_entry_shadowing_the_user_entry_is_a_warning() {
    let sb = installed();
    let mut json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.claude_json()).unwrap()).unwrap();
    json["projects"] = serde_json::json!({
        "/work/proj": {"mcpServers": {"herdr": {"type": "stdio", "command": "/usr/bin/other"}}}
    });
    fs::write(sb.claude_json(), json.to_string()).unwrap();

    let report = status(&sb);
    assert_eq!(report.code, 0);
    assert!(report.out.contains("claude: ours"), "{}", report.out);
    assert!(report.out.contains("warning"), "{}", report.out);
    assert!(report.out.contains("/work/proj"), "{}", report.out);
}

#[test]
fn status_reports_herdr_integrations_read_only_and_never_installs_them() {
    let sb = Sandbox::new();
    let report = status(&sb);
    assert!(
        report.out.contains("herdr integration install claude"),
        "{}",
        report.out
    );
    assert!(
        report.out.contains("herdr integration install codex"),
        "{}",
        report.out
    );
    assert!(!sb.log().contains("integration install"), "{}", sb.log());

    fs::write(
        sb.home().join(".fake_herdr_status"),
        "claude: installed (/fake/a)\ncodex: installed (/fake/b)\n",
    )
    .unwrap();
    let report = status(&sb);
    assert!(
        !report.out.contains("herdr integration install"),
        "{}",
        report.out
    );
    assert!(report.out.contains("resume"), "{}", report.out);
}

#[test]
fn install_skips_a_provider_whose_cli_is_not_on_path() {
    let mut sb = Sandbox::new();
    let only_claude = sb.root.join("only_claude");
    fs::create_dir_all(&only_claude).unwrap();
    symlink(
        Path::new(FAKE_CLI).join("claude"),
        only_claude.join("claude"),
    )
    .unwrap();
    sb.env.path = OsString::from(format!("{}:/usr/bin:/bin", only_claude.display()));

    let outcome = install(&sb, false);
    assert_eq!(outcome.code, 0, "{}{}", outcome.out, outcome.err);
    assert!(
        outcome.out.contains("codex") && outcome.out.contains("not found"),
        "{}",
        outcome.out
    );
    assert!(sb.log().contains("claude mcp add"));
    assert!(!sb.codex_toml().exists());
}

#[test]
fn classifier_reports_global_only_for_our_entry() {
    let sb = Sandbox::new();
    assert_eq!(mcp_status(&sb.env, "claude"), McpStatus::Injectable);
    assert_eq!(mcp_status(&sb.env, "codex"), McpStatus::Injectable);
    assert_eq!(mcp_status(&sb.env, "pi"), McpStatus::Unsupported);

    assert_eq!(install(&sb, false).code, 0);
    assert_eq!(mcp_status(&sb.env, "claude"), McpStatus::Global);
    assert_eq!(mcp_status(&sb.env, "codex"), McpStatus::Global);
    assert_eq!(mcp_status(&sb.env, "pi"), McpStatus::Unsupported);

    let claude = classify(&sb.env, ClientKind::Claude, None).unwrap();
    assert_eq!(claude.state, State::Ours);
    assert_eq!(claude.mode, Some(ToolMode::Read));
    let wanted = classify(&sb.env, ClientKind::Claude, Some(ToolMode::Control)).unwrap();
    assert_eq!(wanted.state, State::Drifted);

    fs::write(
        sb.claude_json(),
        r#"{"mcpServers":{"herdr":{"command":"/usr/bin/other","args":[]}}}"#,
    )
    .unwrap();
    assert_eq!(mcp_status(&sb.env, "claude"), McpStatus::Injectable);
}

mod replace_file {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use herdr_mcp::install::codex::replace_file;

    fn leftovers(dir: &std::path::Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tmp"))
            .collect()
    }

    #[test]
    fn a_symlinked_config_stays_a_symlink_and_its_target_gets_the_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        fs::create_dir(dir.join("dotfiles")).unwrap();
        let target = dir.join("dotfiles/config.toml");
        fs::write(&target, "old").unwrap();
        let link = dir.join("config.toml");
        symlink(&target, &link).unwrap();

        replace_file(&link, "new").unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(leftovers(&dir).is_empty() && leftovers(&dir.join("dotfiles")).is_empty());
    }

    #[test]
    fn file_mode_is_preserved_and_the_temp_file_is_private_until_then() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        replace_file(&path, "new").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn a_new_file_is_created_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        replace_file(&path, "new").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn the_temp_file_is_removed_when_the_rename_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "x").unwrap();

        assert!(replace_file(&path, "new").is_err());

        assert!(
            leftovers(tmp.path()).is_empty(),
            "{:?}",
            leftovers(tmp.path())
        );
    }

    #[test]
    fn a_stale_temp_file_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "old").unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        symlink(&victim, tmp.path().join("config.toml.herdr-mcp.tmp")).unwrap();

        assert!(replace_file(&path, "new").is_err());

        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    }
}
