//! Isolated real installer/watcher cases, retaining raw observations through task completion.
use super::*;
use crate::services::discord::health::legacy_supervision::test_support::{
    MockDiscord, seed_backfill_row, tree_fingerprint,
};
use crate::services::discord::health::legacy_supervision::{RetiredForTest, is_retired};
use crate::services::discord::inflight::o_seed_observation::{Guard, Snapshot};
use crate::services::discord::task_supervisor::{spawn_observed_tmux_watcher, watcher_completion};
use crate::services::discord::tmux::{InstallFixture, OOnlyInstallOutcome};
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

pub(super) fn assert_zero_loaders(snapshot: &Snapshot) {
    assert_eq!(snapshot.writable_inflight_load_calls, 0, "{snapshot:?}");
    assert_eq!(snapshot.readonly_inflight_load_calls, 0, "{snapshot:?}");
    assert_eq!(snapshot.compatibility_backfill_attempts, 0, "{snapshot:?}");
    assert_eq!(snapshot.legacy_prefix_restore_calls, 0, "{snapshot:?}");
    assert_eq!(
        snapshot.event_count("stream_progress_call"),
        0,
        "{snapshot:?}"
    );
}

pub(super) fn assert_rowless(observed: &Guard) {
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    assert_zero_loaders(&key);
    assert_zero_loaders(&raw);
    assert!(key.watcher_initialization_observed, "{key:?}");
    assert!(key.stream_decoder_initialization_observed, "{key:?}");
    assert!(key.event_count("outer_eof") > 0, "{key:?}");
    assert!(key.decoded_source_chunks > 0, "{key:?}");
    assert!(key.event_count("checkpoint") > 0, "{key:?}");
    assert_eq!(key.event_count("capsule_legacy_adopted"), 0, "{key:?}");
    assert!(!key.parser_responses.is_empty(), "{key:?}");
    assert!(
        key.parser_responses
            .iter()
            .all(|response| !response.contains("legacy restored body")
                && !response.contains("predecessor Legacy body")),
        "{key:?}"
    );
    eprintln!("N4D_NONTERMINAL_KEY={key:?}\nN4D_NONTERMINAL_RAW={raw:?}");
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
    handle.cancel.store(true, Ordering::Release);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), ticket.wait())
            .await
            .unwrap(),
        watcher_completion::Outcome::Returned
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_a_installer_eof_then_nonterminal_decode_is_rowless_through_join() {
    if !child("t6_a_installer_eof_then_nonterminal_decode_is_rowless_through_join") {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, discord) = fixture(632_511_001).await;
    let old = format!(
        "{}{}{}",
        user("old prompt"),
        said("legacy source body"),
        stop()
    );
    append(&fixture, old.as_bytes());
    seed(&fixture, old.len() as u64);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(&observed, "actual initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let fresh = said("fresh rowless response");
    append(&fixture, fresh.as_bytes());
    until(&observed, "actual nonterminal chunk", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    fixture.cancel_and_join().await;
    assert_rowless(&observed);
    assert!(
        observed
            .snapshot()
            .parser_responses
            .iter()
            .any(|r| r == "fresh rowless response")
    );
    assert_eq!(tree_fingerprint(&root), before);
    assert!(fixture.shared.core.lock().await.sessions.is_empty());
    eprintln!(
        "N4D_NONTERMINAL_HTTP={:?}",
        discord.requests_for(fixture.channel.get())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_c_installer_reaches_expired_streaming_tick_without_legacy_row_effects() {
    let test = "t6_c_installer_reaches_expired_streaming_tick_without_legacy_row_effects";
    if !isolated_in(
        "o_seed_install_tests",
        test,
        &[
            ("AGENTDESK_STATUS_INTERVAL_SECS", "1"),
            ("AGENTDESK_SINGLE_MESSAGE_PANEL", "0"),
        ],
    ) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (mut fixture, discord) = fixture(632_511_003).await;
    fixture.registry.provider_entries_guard().await.clear();
    Arc::get_mut(&mut fixture.shared)
        .unwrap()
        .ui
        .status_panel_v2_enabled = true;
    fixture
        .registry
        .register("claude".into(), fixture.shared.clone())
        .await;
    // An undelivered old row keeps its offset compatible with an accidental write.
    let mut old_row = row(&fixture, 0);
    old_row.response_sent_offset = 0;
    let path = seed_backfill_row(&old_row);
    drop(crate::services::discord::inflight::lock_inflight_state_path(&path).unwrap());
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(&observed, "initial EOF before timed stream", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let fresh = said("timed rowless response");
    let appended_at = tokio::time::Instant::now();
    append(&fixture, fresh.as_bytes());
    until(&observed, "actual expired streaming interval", |s| {
        s.decoded_chunks.concat() == fresh && s.event_count("streaming_tick") > 0
    })
    .await;
    assert!(appended_at.elapsed() >= Duration::from_secs(1));
    fixture.cancel_and_join().await;
    assert_rowless(&observed);
    let snapshot = observed.snapshot();
    assert!(snapshot.event_count("inner_eof") > 0, "{snapshot:?}");
    assert!(
        snapshot
            .parser_responses
            .iter()
            .any(|r| r == "timed rowless response")
    );
    assert_eq!(tree_fingerprint(&root), before);
    eprintln!(
        "N4D_NONTERMINAL_HTTP={:?}",
        discord.requests_for(fixture.channel.get())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_d_real_legacy_predecessor_capsule_is_retained_unadopted() {
    if !child("t6_d_real_legacy_predecessor_capsule_is_retained_unadopted") {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = fixture(632_511_004).await;
    let old_row = seed(&fixture, 0);
    let restored = restored_watcher_turn_from_inflight(&old_row, &fixture.session, false).unwrap();
    let predecessor_observed = observe(&fixture);
    let predecessor = spawn_legacy(&fixture, 0, Some(restored));
    until(&predecessor_observed, "Legacy predecessor EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let old = said("predecessor Legacy body");
    append(&fixture, old.as_bytes());
    until(&predecessor_observed, "Legacy predecessor decode", |s| {
        s.decoded_chunks.concat() == old
    })
    .await;
    cancel_join(&predecessor).await;
    drop(predecessor_observed);
    let retained =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    let legacy = retained
        .iter()
        .filter(|p| p.origin.is_legacy())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(legacy.len(), 1, "{retained:?}");
    assert!(
        legacy[0].body.contains("predecessor Legacy body"),
        "{legacy:?}"
    );
    assert!(
        legacy[0].has_identity && legacy[0].has_startup_snapshot,
        "{legacy:?}"
    );
    assert_eq!(
        legacy[0].identity,
        Some(crate::services::discord::inflight::InflightTurnIdentity::from_state(&old_row))
    );
    assert_eq!(legacy[0].restored_response_seed, old_row.full_response);
    assert_eq!(legacy[0].nonce.as_deref(), Some("old-seed-nonce"));
    assert_ne!(legacy[0].authority.generation_mtime_ns, 0);
    assert_eq!(legacy[0].opened_source, legacy[0].authority.source_file);
    seed(&fixture, std::fs::metadata(&fixture.output).unwrap().len());
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(
        &observed,
        "successor EOF after rejecting Legacy capsule",
        |s| s.event_count("outer_eof") > 0,
    )
    .await;
    let fresh = said("successor O body");
    append(&fixture, fresh.as_bytes());
    until(&observed, "successor fresh decode", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    fixture.cancel_and_join().await;
    let remaining =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(
        remaining
            .into_iter()
            .filter(|p| p.origin.is_legacy())
            .collect::<Vec<_>>(),
        legacy,
        "retirement must neither adopt nor remove Legacy custody"
    );
    assert_rowless(&observed);
    assert!(
        observed
            .snapshot()
            .parser_responses
            .iter()
            .any(|r| r == "successor O body")
    );
    assert_eq!(tree_fingerprint(&root), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_e_installed_successor_resumes_real_o_partial_utf8_capsule() {
    if !child("t6_e_installed_successor_resumes_real_o_partial_utf8_capsule") {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = fixture(632_511_005).await;
    seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(&observed, "predecessor initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let fresh = said("안녕 O successor");
    let split = fresh.find('안').unwrap() + 1;
    append(&fixture, &fresh.as_bytes()[..split]);
    until(&observed, "predecessor partial UTF8 read", |s| {
        s.decoded_source_chunks > 0
    })
    .await;
    fixture.cancel_and_join().await;
    let retained =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(retained.len(), 1, "{retained:?}");
    let capsule = &retained[0];
    assert!(
        !capsule.origin.is_legacy() && capsule.has_utf8_carry,
        "{retained:?}"
    );
    assert_eq!(capsule.offset, split as u64);
    assert!(
        !capsule.has_identity
            && capsule.nonce.is_none()
            && !capsule.has_startup_snapshot
            && !capsule.has_restored_seed,
        "{retained:?}"
    );
    assert!(
        capsule.identity.is_none()
            && capsule.startup_snapshot.is_none()
            && capsule.restored_response_seed.is_empty(),
        "{retained:?}"
    );
    assert_ne!(capsule.authority.generation_mtime_ns, 0);
    assert_eq!(capsule.opened_source, capsule.authority.source_file);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(&observed, "real O custody adoption", |s| {
        s.event_count("capsule_o_adopted") == 1
    })
    .await;
    append(&fixture, &fresh.as_bytes()[split..]);
    until(&observed, "successor completes carried scalar", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    fixture.cancel_and_join().await;
    assert_rowless(&observed);
    let snapshot = observed.snapshot();
    assert_eq!(snapshot.event_count("capsule_o_adopted"), 1);
    assert!(
        snapshot
            .parser_responses
            .iter()
            .any(|r| r == "안녕 O successor"),
        "{snapshot:?}"
    );
    let final_capsules =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(final_capsules.len(), 1, "{final_capsules:?}");
    let final_capsule = &final_capsules[0];
    assert_eq!(final_capsule.body, "안녕 O successor", "{final_capsules:?}");
    assert!(!final_capsule.origin.is_legacy(), "{final_capsules:?}");
    assert!(
        !final_capsule.has_identity
            && final_capsule.identity.is_none()
            && final_capsule.nonce.is_none(),
        "checkpoint startup fallback imported identity/nonce: {final_capsules:?}"
    );
    assert!(
        !final_capsule.has_startup_snapshot
            && final_capsule.startup_snapshot.is_none()
            && !final_capsule.has_turn_identity_for_panel
            && final_capsule.restored_response_seed.is_empty(),
        "O checkpoint must sanitize continuation fields: {final_capsules:?}"
    );
    assert_eq!(tree_fingerprint(&root), before);
}

async fn legacy_control(channel: u64, n1_only: bool) {
    let (fixture, _discord) = fixture(channel).await;
    let _n1 = n1_only.then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(channel));
    assert!(!is_retired("claude", channel));
    let old_row = seed(&fixture, 0);
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let restored = restored_watcher_turn_from_inflight(&old_row, &fixture.session, false).unwrap();
    let observed = observe(&fixture);
    assert_eq!(
        fixture.install().await,
        OOnlyInstallOutcome::Deferred(
            crate::services::discord::tmux::OOnlyInstallReason::NotRetired
        )
    );
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
    eprintln!("N4D_LEGACY_CONTROL_N1={n1_only} KEY={key:?} RAW={raw:?}");
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
    assert!(!is_retired("codex", channel));
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
    assert_eq!(
        fixture.install().await,
        OOnlyInstallOutcome::Deferred(
            crate::services::discord::tmux::OOnlyInstallReason::NotRetired
        )
    );
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
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
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
    eprintln!("N4D_NATIVE_LEGACY_CONTROL_N1={n1_only} KEY={key:?} RAW={raw:?}");
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
async fn retired_shared_loaders_preserve_independent_read_and_backfill_contracts() {
    let test = "retired_shared_loaders_preserve_independent_read_and_backfill_contracts";
    if !child(test) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = fixture(632_511_011).await;
    let expected = seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    assert!(is_retired("claude", fixture.channel.get()));
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    // This independent loader contract runs outside the installer's rowless watcher observation.
    let observed = Guard::new(&fixture.provider, fixture.channel.get());
    let read_only = crate::services::discord::inflight::load_inflight_state_read_only_result(
        &fixture.provider,
        fixture.channel.get(),
    )
    .unwrap()
    .expect("retirement cannot hide a readable independent row");
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
    .expect("retirement cannot reject the independent writable loader");
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
    assert_eq!(raw, key, "raw entry calls cannot be filtered by retirement");
    assert_eq!(raw.readonly_inflight_load_calls, 2, "{raw:?}");
    assert_eq!(raw.writable_inflight_load_calls, 1, "{raw:?}");
    assert_eq!(raw.compatibility_backfill_attempts, 1, "{raw:?}");
    assert_eq!(raw.legacy_prefix_restore_calls, 0, "{raw:?}");
    assert!(!raw.watcher_initialization_observed);
    assert!(!raw.stream_decoder_initialization_observed);
    eprintln!("RETIRED_INDEPENDENT_LOADER_KEY={key:?}\nRETIRED_INDEPENDENT_LOADER_RAW={raw:?}");
}
