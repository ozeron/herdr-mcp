#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fake_herdr::{FakeHerdr, Script};
use harness::{Harness, err, err_code, ok};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::ServiceExt;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use serde_json::{Value, json};

async fn start(mode: ToolMode, pane_cwd: &str) -> Harness {
    let fake = FakeHerdr::start().await;
    let mut snapshot = harness::snapshot(None);
    for pane in snapshot["snapshot"]["panes"].as_array_mut().unwrap() {
        pane["cwd"] = json!(pane_cwd);
    }
    fake.on_params("session.snapshot", json!({}), Script::Reply(snapshot));
    let log_dir = tempfile::tempdir().unwrap();
    let config = BridgeConfig {
        socket_path: fake.path().to_path_buf(),
        profile: Profile::new(None, None),
        mode,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv::default(),
        logger: Logger::to_dir(log_dir.path().join("logs")).unwrap(),
        install_env: None,
    };
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let running = HerdrMcp::new(config).serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("test-client", "1.0.0"),
    )
    .serve(client_io)
    .await
    .unwrap();
    Harness {
        fake,
        client,
        _log_dir: log_dir,
    }
}

/// A file outside the temp directory and outside `/work`, the panes' directory.
fn outside_file() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml")
        .display()
        .to_string()
}

fn read(path: &Path, extra: Value) -> Value {
    let mut args = json!({"path": path.display().to_string()});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    args
}

#[tokio::test]
async fn reads_a_text_file_under_the_temp_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.md");
    std::fs::write(&file, "# Report\nall good\n").unwrap();
    let h = start(ToolMode::Control, "/work").await;

    let out = ok(&h, "herdr_file_read", read(&file, json!({}))).await;

    assert_eq!(out["content"], "# Report\nall good\n");
    assert_eq!(out["encoding"], "utf8");
    assert_eq!(out["size"], 18);
    assert_eq!(out["bytes"], 18);
    assert_eq!(out["eof"], true);
    assert!(out["next_offset"].is_null());
}

#[tokio::test]
async fn pages_end_on_whole_characters_and_continue_from_next_offset() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("text.txt");
    std::fs::write(&file, "aé€b").unwrap();
    let h = start(ToolMode::Control, "/work").await;

    // 4 bytes stop inside the 3-byte euro sign, so the page ends before it.
    let first = ok(&h, "herdr_file_read", read(&file, json!({"max_bytes": 4}))).await;
    assert_eq!(first["content"], "aé");
    assert_eq!(first["bytes"], 3);
    assert_eq!(first["next_offset"], 3);
    assert_eq!(first["eof"], false);

    let second = ok(
        &h,
        "herdr_file_read",
        read(&file, json!({"offset": 3, "max_bytes": 4})),
    )
    .await;
    assert_eq!(second["content"], "€b");
    assert_eq!(second["eof"], true);
}

#[tokio::test]
async fn binary_comes_back_as_base64() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("chart.png");
    let bytes = [0x89, b'P', b'N', b'G', 0xff, 0x00];
    std::fs::write(&file, bytes).unwrap();
    let h = start(ToolMode::Control, "/work").await;

    let out = ok(&h, "herdr_file_read", read(&file, json!({}))).await;

    assert_eq!(out["encoding"], "base64");
    assert_eq!(
        STANDARD.decode(out["content"].as_str().unwrap()).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn a_file_under_a_panes_working_directory_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.csv");
    std::fs::write(&file, "a,b\n").unwrap();
    let h = start(ToolMode::Control, &dir.path().display().to_string()).await;
    assert_eq!(
        ok(&h, "herdr_file_read", read(&file, json!({}))).await["content"],
        "a,b\n"
    );
    let manifest = outside_file();
    let pane_dir = Path::new(&manifest).parent().unwrap().display().to_string();
    let h = start(ToolMode::Control, &pane_dir).await;
    assert_eq!(
        ok(&h, "herdr_file_read", json!({"path": manifest})).await["encoding"],
        "utf8"
    );
}

#[tokio::test]
async fn a_file_outside_every_root_is_refused_and_panes_outside_home_are_no_root() {
    let h = start(ToolMode::Control, "/work").await;
    let out = err(&h, "herdr_file_read", json!({"path": outside_file()})).await;
    assert_eq!(out["code"], "path_not_allowed");
    assert!(
        out["roots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|root| !root.as_str().unwrap().ends_with("/work")),
        "{out}"
    );
}

#[tokio::test]
async fn a_pane_in_the_root_or_home_directory_opens_nothing() {
    for cwd in ["/", &std::env::var("HOME").unwrap()] {
        let h = start(ToolMode::Control, cwd).await;
        assert_eq!(
            err_code(&h, "herdr_file_read", json!({"path": outside_file()})).await,
            "path_not_allowed",
            "{cwd}"
        );
    }
}

#[tokio::test]
async fn a_symlink_out_of_a_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("escape");
    std::os::unix::fs::symlink(outside_file(), &link).unwrap();
    let h = start(ToolMode::Control, "/work").await;
    assert_eq!(
        err_code(&h, "herdr_file_read", read(&link, json!({}))).await,
        "path_not_allowed"
    );
}

#[tokio::test]
async fn credential_directories_are_refused_under_any_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".ssh")).unwrap();
    let key = dir.path().join(".ssh/id_ed25519");
    std::fs::write(&key, "secret").unwrap();
    let h = start(ToolMode::Control, "/work").await;
    let out = err(&h, "herdr_file_read", read(&key, json!({}))).await;
    assert_eq!(out["code"], "path_not_allowed");
    assert!(!out.to_string().contains("secret\""), "{out}");
}

#[tokio::test]
async fn missing_files_directories_and_bad_offsets_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("short.txt");
    std::fs::write(&file, "abc").unwrap();
    let h = start(ToolMode::Control, "/work").await;

    assert_eq!(
        err_code(
            &h,
            "herdr_file_read",
            read(&dir.path().join("nope"), json!({}))
        )
        .await,
        "file_not_found"
    );
    assert_eq!(
        err_code(&h, "herdr_file_read", read(dir.path(), json!({}))).await,
        "not_a_file"
    );
    assert_eq!(
        err_code(&h, "herdr_file_read", read(&file, json!({"offset": 9}))).await,
        "invalid_params"
    );
}

#[tokio::test]
async fn read_mode_has_no_file_read_tool() {
    let h = start(ToolMode::Read, "/work").await;
    let tools = h.client.list_all_tools().await.unwrap();
    assert!(tools.iter().all(|tool| tool.name != "herdr_file_read"));
}

#[tokio::test]
async fn a_pane_in_a_system_or_hidden_home_directory_opens_nothing() {
    let home = std::env::var("HOME").unwrap();
    for cwd in [
        "/private".to_owned(),
        "/etc".to_owned(),
        format!("{home}/Library"),
        format!("{home}/.config"),
    ] {
        let h = start(ToolMode::Control, &cwd).await;
        assert_eq!(
            err_code(&h, "herdr_file_read", json!({"path": "/etc/hosts"})).await,
            "path_not_allowed",
            "{cwd}"
        );
    }
}

#[tokio::test]
async fn env_files_and_git_config_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env.local"), "TOKEN=x").unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/config"), "[remote]").unwrap();
    let h = start(ToolMode::Control, "/work").await;
    for name in [".env.local", ".git/config"] {
        assert_eq!(
            err_code(
                &h,
                "herdr_file_read",
                read(&dir.path().join(name), json!({}))
            )
            .await,
            "path_not_allowed",
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_relative_path_is_invalid() {
    let h = start(ToolMode::Control, "/work").await;
    assert_eq!(
        err_code(&h, "herdr_file_read", json!({"path": "Cargo.toml"})).await,
        "invalid_params"
    );
}

#[tokio::test]
async fn a_page_smaller_than_a_character_takes_the_whole_character() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("euro.txt");
    std::fs::write(&file, "€€").unwrap();
    let h = start(ToolMode::Control, "/work").await;

    let out = ok(&h, "herdr_file_read", read(&file, json!({"max_bytes": 1}))).await;

    assert_eq!(out["encoding"], "utf8");
    assert_eq!(out["content"], "€");
    assert_eq!(out["next_offset"], 3);
}

#[tokio::test]
async fn an_offset_inside_a_character_starts_at_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("euro.txt");
    std::fs::write(&file, "€ab").unwrap();
    let h = start(ToolMode::Control, "/work").await;

    let out = ok(&h, "herdr_file_read", read(&file, json!({"offset": 1}))).await;

    assert_eq!(out["content"], "ab");
    assert_eq!(out["offset"], 3);
    assert!(out["note"].as_str().unwrap().contains("inside a character"));
    assert_eq!(out["eof"], true);
}

#[tokio::test]
async fn the_refusal_names_the_path_as_asked_not_a_symlink_target() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("escape");
    std::os::unix::fs::symlink(outside_file(), &link).unwrap();
    let h = start(ToolMode::Control, "/work").await;
    let out = err(&h, "herdr_file_read", read(&link, json!({}))).await;
    assert!(!out.to_string().contains("Cargo.toml"), "{out}");
}
