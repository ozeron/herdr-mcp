#[path = "support/fake_herdr.rs"]
mod fake_herdr;
#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use fake_herdr::{FakeHerdr, Script};
use harness::{count, process_info, snapshot};
use herdr_mcp::caller::{CallerEnv, StaticAncestry};
use herdr_mcp::cli::ToolMode;
use herdr_mcp::guard::TargetKind;
use herdr_mcp::herdr::{HerdrError, SessionEnv};
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::scope::{Scope, TargetRequest};
use herdr_mcp::server::Inner;
use herdr_mcp::sessions::Sessions;
use herdr_mcp::tools::launch::LaunchTuning;
use herdr_mcp::wait::Tuning;
use serde_json::{Value, json};
use tempfile::TempDir;

const PANE_ONLY: &[TargetKind] = &[TargetKind::Pane];
const PANE_OR_TAB: &[TargetKind] = &[TargetKind::Pane, TargetKind::Tab];

struct Rig {
    fake: FakeHerdr,
    inner: Inner,
    _dir: TempDir,
}

fn inner(socket: &std::path::Path, env_pane: Option<&str>, dir: &TempDir) -> Inner {
    Inner {
        sessions: Sessions::new(socket.to_path_buf(), SessionEnv::default()),
        profile: Profile::new(None, None),
        mode: ToolMode::Control,
        ancestry: Arc::new(StaticAncestry(vec![9000, 4242, 1])),
        caller_env: CallerEnv {
            pane_id: env_pane.map(Into::into),
            tab_id: env_pane.map(|_| "w1:t1".into()),
            workspace_id: env_pane.map(|_| "w1".into()),
        },
        logger: Logger::to_dir(dir.path().join("logs")).unwrap(),
        install_env: None,
        wait: Tuning::default(),
        interrupt_settle: Duration::from_millis(1),
        launch: LaunchTuning::default(),
        mailbox_state: dir.path().join("state"),
    }
}

/// The caller sits in `w1:p1`; its shell is an ancestor of the bridge, so it is verified.
async fn verified() -> Rig {
    let rig = rig(Some("w1:p1")).await;
    rig.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p1", 4242, 4242)]);
    rig
}

/// The env names `w1:p1`, but no pane's shell is an ancestor.
async fn unverified() -> Rig {
    let rig = rig(Some("w1:p1")).await;
    rig.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p1", 5555, 5555)]);
    rig
}

async fn rig(env_pane: Option<&str>) -> Rig {
    let fake = FakeHerdr::start().await;
    let dir = tempfile::tempdir().unwrap();
    let inner = inner(fake.path(), env_pane, &dir);
    fake.reply_seq("session.snapshot", vec![snapshot(Some("w1:p1"))]);
    Rig {
        fake,
        inner,
        _dir: dir,
    }
}

fn agent_info(pane: &str, name: &str) -> Value {
    json!({"type": "agent_info", "agent": harness::agent(pane, "w1:t2", "w1", name)})
}

fn pane_request(target: &str) -> TargetRequest<'_> {
    TargetRequest::new(
        target,
        PANE_ONLY,
        "herdr_test takes a pane ID or an agent name",
    )
}

fn tool_error(err: &HerdrError) -> (&str, String) {
    (err.code(), err.to_string())
}

#[tokio::test]
async fn a_pane_id_resolves_to_that_pane_with_its_agent() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let agent_pane = scope.target(pane_request("w1:p4")).await.unwrap();
    let plain_pane = scope.target(pane_request("w1:p3")).await.unwrap();

    assert_eq!(agent_pane.target.id, "w1:p4");
    assert_eq!(agent_pane.agent().unwrap().name.as_deref(), Some("rev"));
    assert_eq!(plain_pane.target.id, "w1:p3");
    assert!(plain_pane.agent().is_none());
    assert!(agent_pane.resolution.caller_verified);
}

#[tokio::test]
async fn an_agent_name_resolves_to_its_pane() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let found = scope.target(pane_request("rev")).await.unwrap();

    assert_eq!(found.target.id, "w1:p4");
}

#[tokio::test]
async fn a_name_missing_from_the_snapshot_falls_back_to_the_alias_lookup() {
    let rig = verified().await;
    rig.fake.reply_params(
        "agent.get",
        json!({"target": "old-name"}),
        agent_info("w1:p4", "rev"),
    );
    let mut scope = Scope::open(&rig.inner).unwrap();

    let found = scope.target(pane_request("old-name")).await.unwrap();

    assert_eq!(found.target.id, "w1:p4");
    assert_eq!(count(&rig.fake, "agent.get"), 1);
}

#[tokio::test]
async fn an_unknown_name_is_target_not_found() {
    let rig = verified().await;
    rig.fake.on(
        "agent.get",
        Script::error("agent_not_found", "no such agent"),
    );
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope.target(pane_request("nobody")).await.unwrap_err();

    assert_eq!(err.code(), "target_not_found");
}

#[tokio::test]
async fn the_verified_callers_own_pane_is_refused_unless_allowed() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope.target(pane_request("w1:p1")).await.unwrap_err();
    let allowed = scope
        .target(pane_request("w1:p1").allow_self(true))
        .await
        .unwrap();

    assert_eq!(err.code(), "self_target");
    assert_eq!(allowed.target.id, "w1:p1");
}

#[tokio::test]
async fn an_unverified_caller_still_guards_the_env_named_pane() {
    let rig = unverified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope.target(pane_request("w1:p1")).await.unwrap_err();
    let other = scope.target(pane_request("w1:p3")).await.unwrap();

    assert_eq!(err.code(), "self_target");
    assert!(!other.resolution.caller_verified);
    assert_eq!(other.resolution.guard_target.as_deref(), Some("w1:p1"));
}

#[tokio::test]
async fn a_tab_holding_the_callers_pane_is_refused_for_self_targeting() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();
    let request = |target| TargetRequest::new(target, PANE_OR_TAB, "unused");

    let err = scope.target(request("w1:t1")).await.unwrap_err();
    let other_tab = scope.target(request("w1:t2")).await.unwrap();
    let allowed = scope
        .target(request("w1:t1").allow_self(true))
        .await
        .unwrap();

    assert_eq!(err.code(), "self_target");
    assert_eq!(other_tab.target.pane_ids, vec!["w1:p3", "w1:p4"]);
    assert_eq!(allowed.target.kind, TargetKind::Tab);
}

#[tokio::test]
async fn a_tab_for_a_pane_only_request_is_refused_with_the_requests_code_and_message() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let tool = scope.target(pane_request("w1:t2")).await.unwrap_err();
    let place = scope
        .target(
            TargetRequest::new(
                "w1:t2",
                PANE_ONLY,
                "where.pane takes a pane ID or an agent name",
            )
            .refusal_code("invalid_params"),
        )
        .await
        .unwrap_err();

    assert_eq!(
        tool_error(&tool),
        (
            "unsupported_target",
            "herdr_test takes a pane ID or an agent name".to_string()
        )
    );
    assert_eq!(
        tool_error(&place),
        (
            "invalid_params",
            "where.pane takes a pane ID or an agent name".to_string()
        )
    );
}

#[tokio::test]
async fn a_workspace_is_refused_where_only_panes_and_tabs_are_accepted() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope
        .target(TargetRequest::new(
            "w2",
            PANE_OR_TAB,
            "close takes a pane or a tab",
        ))
        .await
        .unwrap_err();

    assert_eq!(err.code(), "unsupported_target");
}

#[tokio::test]
async fn the_guard_order_decides_which_refusal_a_callers_tab_gets() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let kind_first = scope.target(pane_request("w1:t1")).await.unwrap_err();
    let self_first = scope
        .target(pane_request("w1:t1").self_guard_first())
        .await
        .unwrap_err();
    let other_tab = scope
        .target(pane_request("w1:t2").self_guard_first())
        .await
        .unwrap_err();

    assert_eq!(kind_first.code(), "unsupported_target");
    assert_eq!(self_first.code(), "self_target");
    assert_eq!(other_tab.code(), "unsupported_target");
}

#[tokio::test]
async fn the_provenance_guard_lists_panes_no_bridge_launched_and_force_lets_them_through() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();
    let request = |target| TargetRequest::new(target, PANE_OR_TAB, "unused");

    let err = scope
        .target(request("w1:p2").provenance(false))
        .await
        .unwrap_err();
    let tab = scope
        .target(request("w1:t1").allow_self(true).provenance(false))
        .await
        .unwrap_err();
    let forced = scope
        .target(request("w1:p2").provenance(true))
        .await
        .unwrap();
    let launched = scope
        .target(request("w1:t2").provenance(false))
        .await
        .unwrap();
    let unguarded = scope.target(request("w1:p2")).await.unwrap();

    assert_eq!(err.code(), "not_launched_by_bridge");
    assert!(err.to_string().contains("(w1:p2)"), "{err}");
    assert!(tab.to_string().contains("w1:p2"), "{tab}");
    assert_eq!(forced.target.id, "w1:p2");
    assert_eq!(launched.target.id, "w1:t2");
    assert_eq!(unguarded.target.id, "w1:p2");
}

#[tokio::test]
async fn the_self_guard_runs_before_the_provenance_guard() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope
        .target(pane_request("w1:p1").provenance(false))
        .await
        .unwrap_err();

    assert_eq!(err.code(), "self_target");
}

#[tokio::test]
async fn an_unknown_plain_name_resolves_to_no_target_when_tolerant() {
    let rig = verified().await;
    rig.fake
        .on_seq("agent.get", vec![Script::error("agent_not_found", "gone")]);
    let mut scope = Scope::open(&rig.inner).unwrap();

    let nobody = scope.target_or_none(pane_request("scribe")).await.unwrap();
    let known = scope.target_or_none(pane_request("rev")).await.unwrap();
    let own = scope
        .target_or_none(pane_request("w1:p1"))
        .await
        .unwrap_err();

    assert!(nobody.target.is_none());
    assert!(nobody.agent().is_none());
    assert_eq!(nobody.resolution.guard_target.as_deref(), Some("w1:p1"));
    assert_eq!(known.target.unwrap().id, "w1:p4");
    assert_eq!(own.code(), "self_target");
}

#[tokio::test]
async fn an_unknown_id_shaped_string_still_fails_when_tolerant() {
    let rig = verified().await;
    rig.fake
        .on_seq("agent.get", vec![Script::error("agent_not_found", "gone")]);
    let mut scope = Scope::open(&rig.inner).unwrap();

    for id in ["w9:p9", "w9:t1", "w9"] {
        let err = scope.target_or_none(pane_request(id)).await.unwrap_err();
        assert_eq!(err.code(), "target_not_found", "{id}");
    }
}

#[tokio::test]
async fn a_tolerant_lookup_still_surfaces_other_alias_lookup_errors() {
    let rig = verified().await;
    rig.fake
        .on_seq("agent.get", vec![Script::error("internal_error", "boom")]);
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope
        .target_or_none(pane_request("scribe"))
        .await
        .unwrap_err();

    assert_eq!(err.code(), "internal_error");
}

#[tokio::test]
async fn a_scope_over_a_known_caller_takes_only_a_snapshot() {
    let rig = verified().await;
    let resolution = {
        let mut first = Scope::open(&rig.inner).unwrap();
        first.caller().await.unwrap().clone()
    };
    let before = count(&rig.fake, "pane.process_info");
    let client = rig.inner.client().unwrap();
    let mut scope = Scope::with_caller(client, resolution);

    let err = scope.target(pane_request("w1:p1")).await.unwrap_err();
    let ok = scope.target(pane_request("w1:p3")).await.unwrap();

    assert_eq!(err.code(), "self_target");
    assert_eq!(ok.target.id, "w1:p3");
    assert_eq!(count(&rig.fake, "pane.process_info"), before);
    assert_eq!(count(&rig.fake, "session.snapshot"), 2);
}

#[tokio::test]
async fn a_caller_resolved_once_is_not_resolved_again() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    scope.target(pane_request("w1:p3")).await.unwrap();
    scope.target(pane_request("w1:p4")).await.unwrap();

    assert_eq!(count(&rig.fake, "pane.process_info"), 1);
}

#[tokio::test]
async fn the_known_pane_entry_with_no_target_name_takes_no_snapshot() {
    let rig = rig(Some("w1:p1")).await;
    rig.fake
        .reply_seq("pane.process_info", vec![process_info("w1:p1", 4242, 4242)]);
    rig.fake.on_seq(
        "session.snapshot",
        vec![Script::error("internal_error", "no snapshot")],
    );
    let mut scope = Scope::open(&rig.inner).unwrap();

    let guard = scope.guard_pane("w1:p3", false, None).await.unwrap();

    assert!(guard.snapshot.is_none());
    assert!(guard.resolution.caller_verified);
    assert_eq!(count(&rig.fake, "session.snapshot"), 0);
}

#[tokio::test]
async fn the_known_pane_entry_refuses_the_callers_pane_unless_allowed() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope.guard_pane("w1:p1", false, None).await.unwrap_err();
    let allowed = scope.guard_pane("w1:p1", true, None).await.unwrap();

    assert_eq!(err.code(), "self_target");
    assert!(allowed.resolution.caller_verified);
}

#[tokio::test]
async fn the_known_pane_entry_checks_a_target_name_against_the_pane() {
    let rig = verified().await;
    let mut scope = Scope::open(&rig.inner).unwrap();

    let same = scope.guard_pane("w1:p4", false, Some("rev")).await.unwrap();
    let other = scope
        .guard_pane("w1:p4", false, Some("w1:p3"))
        .await
        .unwrap_err();

    assert!(same.snapshot.is_some());
    assert_eq!(other.code(), "invalid_params");
    assert_eq!(
        other.to_string(),
        "target is not the pane this turn token belongs to"
    );
}

#[tokio::test]
async fn the_known_pane_entry_reports_a_caller_error_before_a_snapshot_error() {
    let rig = rig(Some("w1:p1")).await;
    // The caller's process probe comes back late, so the scope's own snapshot is served first.
    rig.fake.on(
        "pane.process_info",
        Script::Delayed(
            Duration::from_millis(40),
            Box::new(Script::Reply(process_info("w1:p1", 5555, 5555))),
        ),
    );
    rig.fake.on_seq(
        "session.snapshot",
        vec![
            Script::error("snapshot_failed", "snapshot"),
            Script::error("caller_failed", "caller"),
        ],
    );
    let mut scope = Scope::open(&rig.inner).unwrap();

    let err = scope
        .guard_pane("w1:p4", false, Some("w1:p4"))
        .await
        .unwrap_err();

    assert_eq!(err.code(), "caller_failed");
}

#[tokio::test]
async fn a_herdr_that_is_not_running_surfaces_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let inner = inner(&dir.path().join("missing.sock"), Some("w1:p1"), &dir);
    let mut scope = Scope::open(&inner).unwrap();

    let target = scope.target(pane_request("w1:p3")).await.unwrap_err();
    let caller = scope.caller().await.unwrap_err();
    let known = scope.guard_pane("w1:p3", false, None).await.unwrap_err();

    for err in [target, caller, known] {
        assert!(matches!(err, HerdrError::NotRunning { .. }), "{err:?}");
    }
}
