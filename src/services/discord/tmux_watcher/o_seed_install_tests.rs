//! Isolated real watcher entry cases, retaining raw observations through task completion.
use super::*;
use crate::services::discord::health::test_support::{
    MockDiscord, seed_backfill_row, tree_fingerprint,
};
use crate::services::discord::inflight::o_seed_observation::{Guard, Snapshot};
use crate::services::discord::task_supervisor::watcher_spawn_observation::Scope;
use crate::services::discord::task_supervisor::{spawn_observed_tmux_watcher, watcher_completion};
use crate::services::discord::tmux::InstallFixture;
use std::io::Write;
use std::path::Path;

fn child(test: &str) -> bool {
    isolated_in("o_seed_install_tests", test, &[])
}

fn pane(state: &str) {
    let root = crate::config::runtime_root().unwrap();
    std::fs::write(root.join("pane.next"), format!("{state}\n")).unwrap();
    std::fs::rename(root.join("pane.next"), root.join("pane")).unwrap();
}

pub(super) async fn fixture(channel: u64) -> (InstallFixture, MockDiscord) {
    fixture_for(CLAUDE, channel).await
}

async fn fixture_for(provider: ProviderKind, channel: u64) -> (InstallFixture, MockDiscord) {
    pane("busy");
    let mut fixture = InstallFixture::new(provider, channel).await;
    let discord = MockDiscord::start().await;
    fixture.http = discord.http.clone();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&fixture.session, "generation"),
        "fixture-generation",
    )
    .unwrap();
    (fixture, discord)
}

pub(super) fn append(fixture: &InstallFixture, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.output)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

pub(super) fn row(fixture: &InstallFixture, offset: u64) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        fixture.provider.clone(),
        fixture.channel.get(),
        Some("n4d-seed".into()),
        7,
        1_001,
        1_002,
        "old prompt".into(),
        Some("old-native-session".into()),
        Some(fixture.session.clone()),
        Some(fixture.output.clone()),
        None,
        offset,
    );
    row.turn_start_offset = Some(0);
    row.turn_nonce = Some("old-seed-nonce".into());
    row.full_response = "legacy restored body".into();
    row.response_sent_offset = row.full_response.len();
    row.last_watcher_relayed_offset = Some(offset);
    row.last_watcher_relayed_generation_mtime_ns =
        Some(dr::current_generation_mtime_ns(&fixture.session));
    row
}

pub(super) fn seed(fixture: &InstallFixture, offset: u64) -> InflightTurnState {
    let row = row(fixture, offset);
    let path = seed_backfill_row(&row);
    drop(crate::services::discord::inflight::lock_inflight_state_path(&path).unwrap());
    row
}

pub(super) async fn until(observed: &Guard, what: &str, done: impl Fn(&Snapshot) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let snapshot = observed.snapshot();
        if done(&snapshot) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out on {what}: key={snapshot:?} raw={:?}",
            observed.raw_process_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(super) fn observe(fixture: &InstallFixture) -> Guard {
    let observed = Guard::new(&fixture.provider, fixture.channel.get());
    observed.register_source(Path::new(&fixture.output));
    observed
}

pub(super) async fn assert_joined(scope: &Scope, observed: &Guard, expected: usize) {
    let joined = scope.join_all().await;
    assert_eq!(scope.submissions(), expected, "raw submissions: {joined:?}");
    assert_eq!(joined.len(), expected);
    assert!(
        joined
            .iter()
            .all(|(_, result)| *result == watcher_completion::Outcome::Returned)
    );
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    assert!(key.event_count("post_stream_exit") >= expected, "{key:?}");
    assert!(raw.event_count("post_stream_exit") >= expected, "{raw:?}");
}

pub(super) fn spawn_legacy(
    fixture: &InstallFixture,
    offset: u64,
    restored: Option<RestoredWatcherTurn>,
) -> crate::services::discord::TmuxWatcherHandle {
    let handle = crate::services::discord::TmuxWatcherHandle {
        tmux_session_name: fixture.session.clone(),
        output_path: fixture.output.clone(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
            crate::services::discord::tmux_watcher_now_ms(),
        )),
    };
    let outcome = claim_or_reuse_watcher_for_host(
        &fixture.shared.tmux_watchers,
        fixture.channel,
        handle,
        &fixture.provider,
        "n4d_legacy_control",
        None,
        WatchHost::Legacy,
    )
    .unwrap();
    assert!(outcome.should_spawn());
    let handle = fixture.handle();
    spawn_observed_tmux_watcher(
        "n4d_legacy_control",
        fixture.shared.clone(),
        fixture.session.clone(),
        handle.cancel.clone(),
        tmux_output_watcher_with_restore(
            fixture.channel,
            fixture.http.clone(),
            fixture.shared.clone(),
            fixture.output.clone(),
            fixture.session.clone(),
            offset,
            handle.cancel.clone(),
            handle.paused.clone(),
            handle.resume_offset.clone(),
            handle.pause_epoch.clone(),
            handle.turn_delivered.clone(),
            handle.last_heartbeat_ts_ms.clone(),
            restored,
        ),
    );
    handle
}

pub(super) async fn cancel_join(handle: &crate::services::discord::TmuxWatcherHandle) {
    let ticket = watcher_completion::observe(&handle.cancel).expect("observed predecessor");
    cancel_for_test(&handle.cancel, Ordering::Release);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), ticket.wait())
            .await
            .unwrap(),
        watcher_completion::Outcome::Returned
    );
}

async fn legacy_control(channel: u64, n1_only: bool) {
    let (fixture, _discord) = fixture(channel).await;
    let _n1 = n1_only.then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(channel));
    let old_row = seed(&fixture, 0);
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let restored = restored_watcher_turn_from_inflight(&old_row, &fixture.session, false).unwrap();
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    let handle = spawn_legacy(&fixture, 0, Some(restored));
    until(&observed, "Legacy control actual EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let fresh = said("Legacy control fresh response");
    append(&fixture, fresh.as_bytes());
    until(&observed, "Legacy control first decode", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    cancel_join(&handle).await;
    assert_joined(&submitted, &observed, 1).await;
    let capsules =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    let capsule = capsules
        .iter()
        .find(|p| p.origin.is_legacy())
        .expect("Legacy checkpoint");
    assert_eq!(
        capsule.identity,
        Some(crate::services::discord::inflight::InflightTurnIdentity::from_state(&old_row))
    );
    assert_eq!(capsule.nonce, old_row.turn_nonce);
    assert_eq!(capsule.restored_response_seed, old_row.full_response);
    assert!(
        capsule.body.contains("legacy restored body")
            && capsule.body.contains("Legacy control fresh response"),
        "{capsules:?}"
    );
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    assert!(key.event_count("legacy_row_seed_adopted") > 0, "{key:?}");
    assert!(raw.event_count("legacy_row_seed_adopted") > 0, "{raw:?}");
    assert!(
        key.watcher_initialization_observed && key.stream_decoder_initialization_observed,
        "{key:?}"
    );
    assert!(
        raw.writable_inflight_load_calls > 0 && raw.readonly_inflight_load_calls > 0,
        "{raw:?}"
    );
    assert!(
        raw.compatibility_backfill_attempts > 0 && raw.legacy_prefix_restore_calls > 0,
        "{raw:?}"
    );
    assert_ne!(
        tree_fingerprint(&root),
        before,
        "Legacy compatibility backfill must remain enabled"
    );
    assert_eq!(key.event_count("capsule_o_adopted"), 0);
    eprintln!("LEGACY_CONTROL_N1={n1_only} KEY={key:?} RAW={raw:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_g_legacy_and_n1_only_actual_watchers_keep_seed_and_backfill() {
    if !child("t6_g_legacy_and_n1_only_actual_watchers_keep_seed_and_backfill") {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    legacy_control(632_511_007, false).await;
    legacy_control(632_511_008, true).await;
}

fn native_said(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item", "payload": {
            "type":"message", "role":"assistant", "phase":"commentary",
            "content":[{"type":"output_text", "text":text}]
        }})
    )
}

async fn native_legacy_control(channel: u64, n1_only: bool) {
    use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
    use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
    let (fixture, _discord) = fixture_for(ProviderKind::Codex, channel).await;
    let _n1 = n1_only.then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(channel));
    let prefix = format!(
        "{}\n{}",
        serde_json::json!({"type":"session_meta", "payload": {
            "id":"native-control", "cwd":"/tmp/n4d-native-control"
        }}),
        native_said("native persisted prefix")
    );
    append(&fixture, prefix.as_bytes());
    let mut old_row = row(&fixture, prefix.len() as u64);
    old_row.runtime_kind = Some(CodexTui);
    old_row.full_response = "legacy row differs from native prefix".into();
    old_row.response_sent_offset = 0;
    seed_backfill_row(&old_row);
    register_tmux_runtime_binding(
        &fixture.session,
        TuiRuntimeBinding {
            runtime_kind: CodexTui,
            output_path: fixture.output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("native-control".into()),
            last_offset: prefix.len() as u64,
            relay_last_offset: None,
        },
    );
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let restored = restored_watcher_turn_from_inflight(&old_row, &fixture.session, false).unwrap();
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    let handle = spawn_legacy(&fixture, prefix.len() as u64, Some(restored));
    until(&observed, "native Legacy initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let fresh = native_said("native control suffix");
    append(&fixture, fresh.as_bytes());
    until(&observed, "native Legacy actual decode", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    cancel_join(&handle).await;
    assert_joined(&submitted, &observed, 1).await;
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    assert!(key.event_count("legacy_row_seed_adopted") > 0, "{key:?}");
    assert!(raw.event_count("legacy_row_seed_adopted") > 0, "{raw:?}");
    assert!(
        key.parser_responses
            .iter()
            .any(|r| r == "native persisted prefix\n\nnative control suffix"),
        "{key:?}"
    );
    assert!(
        key.parser_responses
            .iter()
            .all(|r| !r.contains("legacy row differs from native prefix")),
        "{key:?}"
    );
    assert!(
        raw.writable_inflight_load_calls > 0
            && raw.readonly_inflight_load_calls > 0
            && raw.compatibility_backfill_attempts > 0
            && raw.legacy_prefix_restore_calls > 0,
        "{raw:?}"
    );
    assert_ne!(tree_fingerprint(&root), before);
    eprintln!("NATIVE_LEGACY_CONTROL_N1={n1_only} KEY={key:?} RAW={raw:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_g_native_legacy_and_n1_only_actual_watchers_restore_source_prefix() {
    if !child("t6_g_native_legacy_and_n1_only_actual_watchers_restore_source_prefix") {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    native_legacy_control(632_511_009, false).await;
    native_legacy_control(632_511_010, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_shared_loaders_preserve_independent_read_and_backfill_contracts() {
    let test = "legacy_shared_loaders_preserve_independent_read_and_backfill_contracts";
    if !child(test) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = fixture(632_511_011).await;
    let expected = seed(&fixture, 0);
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    // This independent loader contract runs outside the installer's rowless watcher observation.
    let observed = Guard::new(&fixture.provider, fixture.channel.get());
    let read_only = crate::services::discord::inflight::load_inflight_state_read_only_result(
        &fixture.provider,
        fixture.channel.get(),
    )
    .unwrap()
    .expect("independent row remains readable");
    assert_eq!(read_only.full_response, expected.full_response);
    assert_eq!(read_only.turn_nonce, expected.turn_nonce);
    assert_eq!(tree_fingerprint(&root), before);
    let first = observed.raw_process_snapshot();
    assert_eq!(first.readonly_inflight_load_calls, 1, "{first:?}");
    assert_eq!(first.writable_inflight_load_calls, 0, "{first:?}");
    assert_eq!(first.compatibility_backfill_attempts, 0, "{first:?}");

    let writable = crate::services::discord::inflight::load_inflight_state(
        &fixture.provider,
        fixture.channel.get(),
    )
    .expect("independent writable loader backfills the row");
    assert_eq!(writable.full_response, expected.full_response);
    assert_eq!(writable.turn_nonce, expected.turn_nonce);
    assert_ne!(writable.finalizer_turn_id, 0);
    let backfilled = tree_fingerprint(&root);
    assert_ne!(
        backfilled, before,
        "the compatibility row still backfills durably"
    );
    let path = crate::services::discord::inflight::inflight_state_path(
        &root,
        &fixture.provider,
        fixture.channel.get(),
    );
    let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        disk["finalizer_turn_id"].as_u64(),
        Some(writable.finalizer_turn_id)
    );
    let read_only_after = crate::services::discord::inflight::load_inflight_state_read_only_result(
        &fixture.provider,
        fixture.channel.get(),
    )
    .unwrap()
    .expect("backfilled row remains independently readable");
    assert_eq!(read_only_after.full_response, expected.full_response);
    assert_eq!(
        read_only_after.finalizer_turn_id,
        writable.finalizer_turn_id
    );
    assert_eq!(tree_fingerprint(&root), backfilled);
    let raw = observed.raw_process_snapshot();
    let key = observed.snapshot();
    assert_eq!(raw, key, "raw entry calls match the channel observation");
    assert_eq!(raw.readonly_inflight_load_calls, 2, "{raw:?}");
    assert_eq!(raw.writable_inflight_load_calls, 1, "{raw:?}");
    assert_eq!(raw.compatibility_backfill_attempts, 1, "{raw:?}");
    assert_eq!(raw.legacy_prefix_restore_calls, 0, "{raw:?}");
    assert!(!raw.watcher_initialization_observed);
    assert!(!raw.stream_decoder_initialization_observed);
    eprintln!("LEGACY_INDEPENDENT_LOADER_KEY={key:?}\nLEGACY_INDEPENDENT_LOADER_RAW={raw:?}");
}
