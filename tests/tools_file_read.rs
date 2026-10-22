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

fn read(path: impl AsRef<Path>, extra: Value) -> Value {
    let mut args = json!({"target": "w1:p1", "path": path.as_ref().display().to_string()});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    args
}

/// A pane opened in a fresh directory, and the directory.
async fn pane_in_dir(mode: ToolMode) -> (Harness, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let h = start(mode, &dir.path().display().to_string()).await;
    (h, dir)
}

#[tokio::test]
async fn reads_a_file_by_path_relative_to_the_panes_directory() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    std::fs::create_dir(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("out/report.md"), "# Report\nall good\n").unwrap();

    let out = ok(&h, "herdr_file_read", read("out/report.md", json!({}))).await;

    assert_eq!(out["content"], "# Report\nall good\n");
    assert_eq!(out["encoding"], "utf8");
    assert_eq!(out["size"], 18);
    assert_eq!(out["eof"], true);
    assert!(out["next_offset"].is_null());
}

#[tokio::test]
async fn an_agent_name_targets_its_pane_and_an_absolute_path_inside_works() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
    let mut args = read(dir.path().join("a.txt"), json!({}));
    args["target"] = json!("rev");
    assert_eq!(ok(&h, "herdr_file_read", args).await["content"], "hi");
}

#[tokio::test]
async fn pages_end_on_whole_characters_and_continue_from_next_offset() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    std::fs::write(dir.path().join("t.txt"), "aé€b").unwrap();

    // 4 bytes stop inside the 3-byte euro sign, so the page ends before it.
    let first = ok(
        &h,
        "herdr_file_read",
        read("t.txt", json!({"max_bytes": 4})),
    )
    .await;
    assert_eq!(first["content"], "aé");
    assert_eq!(first["next_offset"], 3);
    assert_eq!(first["eof"], false);

    let second = ok(
        &h,
        "herdr_file_read",
        read("t.txt", json!({"offset": 3, "max_bytes": 4})),
    )
    .await;
    assert_eq!(second["content"], "€b");
    assert_eq!(second["eof"], true);
}

#[tokio::test]
async fn a_page_smaller_than_a_character_takes_the_whole_character() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    std::fs::write(dir.path().join("e.txt"), "€€").unwrap();
    let out = ok(
        &h,
        "herdr_file_read",
        read("e.txt", json!({"max_bytes": 1})),
    )
    .await;
    assert_eq!(out["content"], "€");
    assert_eq!(out["next_offset"], 3);
}

#[tokio::test]
async fn binary_comes_back_as_base64() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    let bytes = [0x89, b'P', b'N', b'G', 0xff, 0x00];
    std::fs::write(dir.path().join("c.png"), bytes).unwrap();
    let out = ok(&h, "herdr_file_read", read("c.png", json!({}))).await;
    assert_eq!(out["encoding"], "base64");
    assert_eq!(
        STANDARD.decode(out["content"].as_str().unwrap()).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn paths_outside_the_panes_directory_are_refused() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path().join("x"), dir.path().join("link")).unwrap();
    let escapes = [
        "../x".to_owned(),
        outside.path().join("x").display().to_string(),
        "link".to_owned(),
        "/etc/hosts".to_owned(),
    ];
    for path in escapes {
        let out = err(&h, "herdr_file_read", read(&path, json!({}))).await;
        assert_eq!(out["code"], "path_not_allowed", "{path}");
        assert!(
            !out.to_string().contains("/x\""),
            "names the path as asked: {out}"
        );
    }
}

#[tokio::test]
async fn a_pane_opened_in_the_root_or_home_directory_reads_nothing() {
    for cwd in ["/".to_owned(), std::env::var("HOME").unwrap()] {
        let h = start(ToolMode::Control, &cwd).await;
        assert_eq!(
            err_code(&h, "herdr_file_read", read("/etc/hosts", json!({}))).await,
            "path_not_allowed",
            "{cwd}"
        );
    }
}

#[tokio::test]
async fn credential_directories_are_refused() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    std::fs::create_dir(dir.path().join(".ssh")).unwrap();
    std::fs::write(dir.path().join(".ssh/id_ed25519"), "secret").unwrap();
    assert_eq!(
        err_code(&h, "herdr_file_read", read(".ssh/id_ed25519", json!({}))).await,
        "path_not_allowed"
    );
}

#[tokio::test]
async fn missing_files_directories_unknown_panes_and_bad_offsets_are_errors() {
    let (h, dir) = pane_in_dir(ToolMode::Control).await;
    std::fs::write(dir.path().join("s.txt"), "abc").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    assert_eq!(
        err_code(&h, "herdr_file_read", read("nope", json!({}))).await,
        "file_not_found"
    );
    assert_eq!(
        err_code(&h, "herdr_file_read", read("sub", json!({}))).await,
        "not_a_file"
    );
    assert_eq!(
        err_code(&h, "herdr_file_read", read("s.txt", json!({"offset": 9}))).await,
        "invalid_params"
    );
    let mut args = read("s.txt", json!({}));
    args["target"] = json!("w9:p9");
    assert_eq!(
        err_code(&h, "herdr_file_read", args).await,
        "target_not_found"
    );
}

#[tokio::test]
async fn read_mode_has_no_file_read_tool() {
    let (h, _dir) = pane_in_dir(ToolMode::Read).await;
    let tools = h.client.list_all_tools().await.unwrap();
    assert!(tools.iter().all(|tool| tool.name != "herdr_file_read"));
}
