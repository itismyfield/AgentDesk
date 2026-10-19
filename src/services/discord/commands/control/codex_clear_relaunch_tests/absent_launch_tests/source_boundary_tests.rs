//! Launch absence leaves verified source authority and its evidence unchanged.

use super::super::CanaryFixture;
use super::*;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
enum SnapshotBody {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
    Other,
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, SnapshotBody)> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, (u32, SnapshotBody)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let kind = metadata.file_type();
        let body = if kind.is_dir() {
            SnapshotBody::Directory
        } else if kind.is_file() {
            SnapshotBody::File(fs::read(path).unwrap())
        } else if kind.is_symlink() {
            SnapshotBody::Symlink(fs::read_link(path).unwrap())
        } else {
            SnapshotBody::Other
        };
        entries.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            (metadata.permissions().mode() & 0o7777, body),
        );
        if kind.is_dir() {
            for child in fs::read_dir(path).unwrap() {
                visit(root, &child.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn mark_absent(fixture: &CanaryFixture) {
    fixture.fake.enable_fixture_launch();
    crate::services::platform::tmux::invalidate_availability_cache();
    fixture.fake.mark_missing();
    fixture.alive.store(false, Ordering::SeqCst);
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    assert_eq!(
        crate::services::platform::tmux::session_presence(CANARY_TMUX),
        crate::services::platform::tmux::SessionPresence::Missing,
    );
    assert_eq!(
        crate::services::platform::tmux::pane_liveness(CANARY_TMUX),
        crate::services::platform::tmux::PaneLiveness::DeadOrAbsent,
    );
    assert!(crate::services::session_backend::process_session_pid(CANARY_TMUX).is_none());
}

fn retirement_path(fixture: &CanaryFixture) -> PathBuf {
    fixture
        .root
        .path()
        .join("runtime/codex_clear_retired")
        .join(format!("{CANARY_CHANNEL}.json"))
}

fn context_path(fixture: &CanaryFixture) -> PathBuf {
    fixture
        .root
        .path()
        .join("runtime/binding_contexts/codex")
        .join(format!("{}.json", fixture.context.execution_nonce))
}

fn assert_no_tmux_effects(fixture: &CanaryFixture) {
    assert!(!fixture.fake.take_calls().iter().any(|call| {
        ["new-session", "kill-session", "send-keys", "paste-buffer"]
            .iter()
            .any(|effect| call.starts_with(effect))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn owned_absence_preserves_source_authority_and_all_evidence_pg() {
    let test = concat!(
        module_path!(),
        "::owned_absence_preserves_source_authority_and_all_evidence_pg"
    );
    if !isolated(test) {
        return;
    }
    let fixture = CanaryFixture::build_with_host(
        0,
        CodexSourceMode::Verified,
        Some("b1-source-boundary-host"),
    )
    .await;
    fixture.fake.enable_fixture_launch();
    crate::services::platform::tmux::invalidate_availability_cache();
    fixture.fake.mark_missing();
    assert!(crate::services::session_backend::process_session_pid(CANARY_TMUX).is_some());
    let observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("absence has no inflight row");
    let with_pid = snapshot(fixture.root.path());
    assert!(
        dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation).is_none(),
        "a tracked process PID must independently refuse absent launch permission",
    );
    assert_eq!(snapshot(fixture.root.path()), with_pid);
    mark_absent(&fixture);
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(fixture.context.execution_nonce.clone()),
    );
    let marker_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ));
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
    assert_eq!(
        marker["codex_ownership"]["execution_nonce"],
        fixture.context.execution_nonce
    );
    assert!(!dedupe::clear_relaunch::vacant(CANARY_TMUX));
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(!retirement_path(&fixture).exists());
    let archive = fixture
        .root
        .path()
        .join("runtime/binding_evidence/codex")
        .join(format!("{}.json", fixture.context.execution_nonce));
    for evidence in [fixture.history(), context_path(&fixture), archive] {
        assert!(!fs::read(evidence).unwrap().is_empty());
    }

    let current_path = fixture.old_source.path.to_str().unwrap();
    let current_before =
        dedupe::codex_verified_source_allowed(CANARY_TMUX, current_path, Some(NATIVE_ID));
    assert!(
        current_before,
        "the real N2 hook proof must authorize its current source"
    );
    let n1_path = fixture
        .context
        .provider_root
        .as_ref()
        .unwrap()
        .join("rollout-unproved-n1.jsonl");
    let n1_at = fixture.context.created_at.to_rfc3339();
    let n1_metadata = json!({"type":"session_meta", "timestamp":n1_at, "payload":{
        "id":NEW_NATIVE_ID, "timestamp":n1_at, "cwd":fixture.root.path(),
        "source":"cli", "originator":"codex_cli_rs",
    }});
    fs::write(&n1_path, format!("{n1_metadata}\n")).unwrap();
    assert!(!dedupe::codex_verified_source_allowed(
        CANARY_TMUX,
        n1_path.to_str().unwrap(),
        Some(NEW_NATIVE_ID)
    ));

    // Legacy source observers may normalize permissions; isolate the predicate's own effects.
    let sessions = marker_path.parent().unwrap();
    fs::set_permissions(sessions, fs::Permissions::from_mode(0o755)).unwrap();
    let before = snapshot(fixture.root.path());
    fixture.fake.take_calls();
    let captured = dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation)
        .expect("exact owned N2 is an absent launch target");
    assert_eq!(captured.nonce, fixture.context.execution_nonce);
    assert_eq!(captured.context, fixture.context);
    assert!(dedupe::clear_relaunch::absent_matches(
        CANARY_TMUX,
        &observation,
        &captured
    ));
    assert_eq!(snapshot(fixture.root.path()), before);
    assert!(!retirement_path(&fixture).exists());
    assert_no_tmux_effects(&fixture);

    assert_eq!(
        dedupe::codex_verified_source_allowed(CANARY_TMUX, current_path, Some(NATIVE_ID)),
        current_before
    );
    assert!(!dedupe::codex_verified_source_allowed(
        CANARY_TMUX,
        n1_path.to_str().unwrap(),
        Some(NEW_NATIVE_ID)
    ));
    assert!(!dedupe::clear_relaunch::vacant(CANARY_TMUX));
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
}

#[tokio::test(flavor = "current_thread")]
async fn absent_predicate_never_creates_missing_sessions_dir_pg() {
    let test = concat!(
        module_path!(),
        "::absent_predicate_never_creates_missing_sessions_dir_pg"
    );
    if !isolated(test) {
        return;
    }
    let fixture = CanaryFixture::build_with_host(
        0,
        CodexSourceMode::Verified,
        Some("b1-source-boundary-host"),
    )
    .await;
    mark_absent(&fixture);
    let observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("absence has no inflight row");
    let sessions = fixture.root.path().join("runtime/sessions");
    fs::rename(
        &sessions,
        fixture.root.path().join("retained-launch-markers"),
    )
    .unwrap();
    let before = snapshot(fixture.root.path());
    fixture.fake.take_calls();
    let captured = dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation)
        .expect("missing launch markers retain the historical verified pin");
    assert_eq!(captured.context, fixture.context);
    assert!(!sessions.exists());
    assert_eq!(snapshot(fixture.root.path()), before);
    assert!(!retirement_path(&fixture).exists());
    assert_no_tmux_effects(&fixture);
    assert!(!dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    assert!(!dedupe::codex_verified_source_allowed(
        CANARY_TMUX,
        fixture.old_source.path.to_str().unwrap(),
        Some(NATIVE_ID),
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn owned_absence_requires_complete_canonical_context_identity_pg() {
    let test = concat!(
        module_path!(),
        "::owned_absence_requires_complete_canonical_context_identity_pg"
    );
    if !isolated(test) {
        return;
    }
    let fixture = CanaryFixture::build_with_host(
        0,
        CodexSourceMode::Verified,
        Some("b1-source-boundary-host"),
    )
    .await;
    mark_absent(&fixture);
    let observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("absence has no inflight row");
    let path = context_path(&fixture);
    let original = fs::read(&path).unwrap();
    assert!(dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation).is_some());
    for (field, replacement) in [
        ("host", Some(json!("another-host"))),
        ("host", None),
        ("owner_runtime_root", Some(json!("another-root"))),
        ("owner_runtime_root", None),
        ("channel_id", Some(json!(CANARY_CHANNEL + 1))),
        (
            "execution_nonce",
            Some(json!(uuid::Uuid::new_v4().simple().to_string())),
        ),
        ("source_policy", Some(json!("legacy"))),
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        if let Some(replacement) = replacement {
            value[field] = replacement;
        } else {
            value.as_object_mut().unwrap().remove(field);
        }
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let before = snapshot(fixture.root.path());
        assert!(
            dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation).is_none(),
            "accepted changed {field}"
        );
        assert_eq!(
            snapshot(fixture.root.path()),
            before,
            "changed evidence for {field}"
        );
        fs::write(&path, &original).unwrap();
        assert!(
            dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation).is_some(),
            "positive control after {field}"
        );
    }
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!retirement_path(&fixture).exists());
    assert_no_tmux_effects(&fixture);
}
