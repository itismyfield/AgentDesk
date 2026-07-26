//! Tests for the watcher-side two-message status panel (#3805 P2 PR-C/D, #4860).

use super::*;
use std::sync::Arc;

#[test]
fn creation_gate_off_is_byte_identical_true_regardless_of_answer() {
    // OFF: the block runs exactly as today whether or not the answer exists.
    assert!(watcher_two_message_panel_creation_gated_by_answer(
        false, false
    ));
    assert!(watcher_two_message_panel_creation_gated_by_answer(
        false, true
    ));
}

#[test]
fn creation_gate_on_defers_until_answer_placeholder_exists() {
    // ON: no answer placeholder yet → defer (create the panel next interval,
    // below the answer). Answer placeholder present → proceed (panel below).
    assert!(!watcher_two_message_panel_creation_gated_by_answer(
        true, false
    ));
    assert!(watcher_two_message_panel_creation_gated_by_answer(
        true, true
    ));
}

fn on_disk(status_message_id: Option<u64>, status_panel_generation: u64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        777,
        None,
        1,
        7_000_001,
        42,
        "hello".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    state.status_message_id = status_message_id;
    state.status_panel_generation = status_panel_generation;
    state
}

#[test]
fn completion_guard_off_and_equal_epoch_are_inert() {
    let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
    let panel = serenity::MessageId::new(20);
    // Default-OFF / PR-C: this-turn epoch equals the on-disk epoch for the
    // owned panel → never superseded (0 == 0 and 1 == 1).
    assert!(!watcher_two_message_status_completion_superseded(
        0,
        Some(panel),
        Some(&on_disk(Some(20), 0)),
    ));
    assert!(!watcher_two_message_status_completion_superseded(
        1,
        Some(panel),
        Some(&on_disk(Some(20), 1)),
    ));
}

#[test]
fn completion_guard_supersedes_only_newer_epoch_for_owned_panel() {
    let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
    let panel = serenity::MessageId::new(20);
    // Newer on-disk epoch for the SAME owned panel → superseded (PR-D/E use).
    assert!(watcher_two_message_status_completion_superseded(
        1,
        Some(panel),
        Some(&on_disk(Some(20), 2)),
    ));
    // On-disk row owns a DIFFERENT panel → not owned by this turn → never
    // superseded, even with a higher epoch.
    assert!(!watcher_two_message_status_completion_superseded(
        1,
        Some(panel),
        Some(&on_disk(Some(99), 9)),
    ));
    // No panel handle / no on-disk row → nothing to supersede.
    assert!(!watcher_two_message_status_completion_superseded(
        1,
        None,
        Some(&on_disk(Some(20), 9)),
    ));
    assert!(!watcher_two_message_status_completion_superseded(
        1,
        Some(panel),
        None
    ));
}

#[test]
fn completion_guard_ignores_synthetic_headless_panel_handle() {
    let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
    // A synthetic-headless id is not a real Discord message → owns no panel,
    // so a higher on-disk epoch never suppresses this completion.
    let synthetic = serenity::MessageId::new(9_100_000_000_000_000_123);
    assert!(crate::services::discord::is_synthetic_headless_message_id_raw(synthetic.get()));
    assert!(!watcher_two_message_status_completion_superseded(
        1,
        Some(synthetic),
        Some(&on_disk(Some(synthetic.get()), 9)),
    ));
}

#[test]
fn reanchor_gate_rejects_managed_bridge_owned_turn_even_when_watcher_relays() {
    let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
    let mut managed = on_disk(Some(20), 1);
    managed.tmux_session_name = Some("AgentDesk-claude-a".to_string());
    managed.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    managed.turn_source = crate::services::discord::inflight::TurnSource::Managed;

    assert!(!watcher_two_message_should_reanchor_panel_on_rollover(
        true,
        true,
        Some(&managed),
        "AgentDesk-claude-a",
    ));

    let mut external = managed.clone();
    external.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    assert!(watcher_two_message_should_reanchor_panel_on_rollover(
        true,
        true,
        Some(&external),
        "AgentDesk-claude-a",
    ));

    external.set_relay_owner_kind(
        crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay,
    );
    assert!(!watcher_two_message_should_reanchor_panel_on_rollover(
        true,
        true,
        Some(&external),
        "AgentDesk-claude-a",
    ));
}

#[test]
fn reanchor_inflight_reload_gate_requires_rollover_and_flag_on() {
    assert!(!watcher_should_load_inflight_for_reanchor(false, false));
    assert!(!watcher_should_load_inflight_for_reanchor(false, true));
    assert!(!watcher_should_load_inflight_for_reanchor(true, false));
    assert!(watcher_should_load_inflight_for_reanchor(true, true));
}

#[test]
fn reanchor_bind_bumps_epoch_atomically_and_guard_stale_skips_old_epoch() {
    // #3805 P2 (PR-D) watcher CAS parity: the re-anchor's atomic rebind
    // (`bind_status_panel` with an expected old panel id + in-lock
    // generation bump) overwrites the OLD panel id AND bumps the epoch under
    // ONE flock. The completion guard
    // then stale-skips a completion carrying the OLD epoch for the re-anchored
    // (owned) panel, while this turn's own completion at the NEW epoch passes.
    let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
    let provider = ProviderKind::Claude;
    let old_panel = serenity::MessageId::new(20);
    let new_panel = serenity::MessageId::new(40);

    // Persist a row that already owns the OLD panel at epoch 1 (as if the
    // watcher created the two-message panel this turn).
    let created = on_disk(Some(old_panel.get()), 1);
    let channel_id = created.channel_id;
    crate::services::discord::inflight::save_inflight_state(&created).expect("persist inflight");

    // Simulate the PR-D re-anchor write: rebind to the NEW panel id + bump the
    // epoch (1 → 2) under the flock, overwriting the OLD id (skip = false).
    let outcome = crate::services::discord::inflight::bind_status_panel(
        &provider,
        channel_id,
        new_panel.get(),
        &crate::services::discord::inflight::StatusPanelBindGuard {
            skip_if_panel_already_set: false,
            require_current_status_message_id: Some(old_panel.get()),
            bump_status_panel_generation: true,
            ..Default::default()
        },
    );
    assert_eq!(outcome.bound_status_panel_generation(), Some(2));

    let after = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
        .expect("reload inflight");
    // The CAS store now owns the NEW panel at the bumped epoch.
    assert_eq!(after.status_message_id, Some(new_panel.get()));
    assert_eq!(after.status_panel_generation, 2);

    // A stale completion at the OLD epoch (1) for the re-anchored (owned)
    // panel is stale-skipped ("이전 위치 stale-skip").
    assert!(watcher_two_message_status_completion_superseded(
        1,
        Some(new_panel),
        Some(&after),
    ));
    // This turn's own completion at the NEW epoch (2) passes ("새 위치 통과").
    assert!(!watcher_two_message_status_completion_superseded(
        2,
        Some(new_panel),
        Some(&after),
    ));
    // A stale completion still pointing at the OLD (now-retired) panel is not
    // gated by the epoch (the row no longer owns it) — the delete/orphan path
    // handles it, not the generation guard.
    assert!(!watcher_two_message_status_completion_superseded(
        1,
        Some(old_panel),
        Some(&after),
    ));
}

#[tokio::test]
async fn status_panel_watcher_wrapper_reclassifies_superseded_pending_bind_4891() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root_for_two_message_tests();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let ui = &mut Arc::get_mut(&mut shared)
        .expect("fresh shared data should be uniquely owned")
        .ui;
    ui.two_message_panel_enabled = true;
    ui.status_panel_v2_enabled = true;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_891_201);
    let completed_panel = serenity::MessageId::new(1_500_000_000_489_201);
    crate::services::discord::status_panel_orphan_store::enqueue_pending_bind(
        &provider,
        &shared.token_hash,
        channel_id.get(),
        completed_panel.get(),
        None,
    )
    .expect("seed actual fallback pending bind");
    let http = serenity::Http::new("test-token");
    let mut last_status_panel_text = String::new();

    complete_watcher_status_panel_v2_with_generation_guard(
        &http,
        &shared,
        channel_id,
        &provider,
        1_700_000_000,
        Some(completed_panel),
        &mut last_status_panel_text,
        false,
        false,
        None,
        true,
        true,
    )
    .await;

    crate::services::discord::status_panel_orphan_store::remove_pending_bind(
        &provider,
        &shared.token_hash,
        channel_id.get(),
        completed_panel.get(),
    );
    assert!(
        crate::services::discord::status_panel_orphan_store::is_queued(
            &provider,
            &shared.token_hash,
            channel_id.get(),
            completed_panel.get(),
        ),
        "the wrapper must turn Superseded PendingBind into durable stranded delete intent"
    );
}

#[test]
fn failed_pending_bind_protection_never_allows_bind_and_preserves_retry_authority_4891() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root_for_two_message_tests();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh shared data should be uniquely owned")
        .ui
        .two_message_panel_enabled = true;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_891_301);
    let candidate = serenity::MessageId::new(1_500_000_000_489_301);
    let prepared = prepare_watcher_two_message_panel(
        true,
        shared.as_ref(),
        &provider,
        channel_id,
        None,
        None,
        "panel",
    )
    .expect("prepare candidate")
    .expect("flag-on intent");
    crate::services::discord::status_panel_transition::fail_next_pending_bind_write_for_test();
    assert!(
        acknowledge_watcher_two_message_panel(
            true,
            shared.as_ref(),
            &provider,
            channel_id,
            Some(&prepared),
            candidate,
        )
        .is_err()
    );
    crate::services::discord::status_panel_orphan_store::enqueue(
        &provider,
        &shared.token_hash,
        channel_id.get(),
        candidate.get(),
    );

    let transient = watcher_status_panel_failed_protection_disposition(
        shared.as_ref(),
        &provider,
        channel_id,
        candidate,
        &crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome::failed(
            "http 500",
        ),
    );
    assert_eq!(
        transient,
        WatcherStatusPanelProtectionDisposition::RetainedForRetry
    );
    assert!(!watcher_status_panel_protection_allows_bind(transient));
    assert!(!watcher_status_panel_protection_allows_bind(
        WatcherStatusPanelProtectionDisposition::Discarded
    ));
    assert!(watcher_status_panel_protection_allows_bind(
        WatcherStatusPanelProtectionDisposition::Protected
    ));
    assert!(
        crate::services::discord::status_panel_orphan_store::is_queued(
            &provider,
            &shared.token_hash,
            channel_id.get(),
            candidate.get(),
        )
    );
    let intents = crate::services::discord::status_panel_transition::load_unreconciled(
        &provider,
        &shared.token_hash,
    )
    .expect("retry intent");
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].candidate_panel_id, Some(candidate.get()));
    assert_eq!(
        intents[0].state,
        crate::services::discord::status_panel_transition::StatusPanelTransitionState::CandidateAcknowledged
    );

    let discarded = watcher_status_panel_failed_protection_disposition(
        shared.as_ref(),
        &provider,
        channel_id,
        candidate,
        &crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
    );
    assert_eq!(
        discarded,
        WatcherStatusPanelProtectionDisposition::Discarded
    );
    assert!(
        !crate::services::discord::status_panel_orphan_store::is_queued(
            &provider,
            &shared.token_hash,
            channel_id.get(),
            candidate.get(),
        )
    );
    assert!(
        crate::services::discord::status_panel_transition::load_unreconciled(
            &provider,
            &shared.token_hash,
        )
        .expect("terminal cleanup")
        .is_empty()
    );

    let absent_candidate = serenity::MessageId::new(1_500_000_000_489_302);
    let absent_prepared = prepare_watcher_two_message_panel(
        true,
        shared.as_ref(),
        &provider,
        channel_id,
        None,
        None,
        "replacement panel",
    )
    .expect("prepare permanent-absence candidate")
    .expect("flag-on permanent-absence intent");
    crate::services::discord::status_panel_transition::fail_next_pending_bind_write_for_test();
    assert!(
        acknowledge_watcher_two_message_panel(
            true,
            shared.as_ref(),
            &provider,
            channel_id,
            Some(&absent_prepared),
            absent_candidate,
        )
        .is_err()
    );
    crate::services::discord::status_panel_orphan_store::enqueue(
        &provider,
        &shared.token_hash,
        channel_id.get(),
        absent_candidate.get(),
    );
    let permanently_absent = watcher_status_panel_failed_protection_disposition(
        shared.as_ref(),
        &provider,
        channel_id,
        absent_candidate,
        &crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome::failed(
            "http 403 forbidden",
        ),
    );
    assert_eq!(
        permanently_absent,
        WatcherStatusPanelProtectionDisposition::Discarded
    );
    assert!(!watcher_status_panel_protection_allows_bind(
        permanently_absent
    ));
    assert!(
        !crate::services::discord::status_panel_orphan_store::is_queued(
            &provider,
            &shared.token_hash,
            channel_id.get(),
            absent_candidate.get(),
        )
    );
    assert!(
        crate::services::discord::status_panel_transition::load_unreconciled(
            &provider,
            &shared.token_hash,
        )
        .expect("permanent-absence cleanup")
        .is_empty()
    );
}

#[test]
fn watcher_orphan_preregistration_is_flag_gated_and_removed_after_persist() {
    let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh shared data should be uniquely owned")
        .ui
        .status_panel_v2_enabled = true;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(777);
    let panel = serenity::MessageId::new(44);

    let prepared = prepare_watcher_two_message_panel(
        false,
        shared.as_ref(),
        &provider,
        channel_id,
        None,
        None,
        "panel",
    )
    .expect("flag-off preparation");
    acknowledge_watcher_two_message_panel(
        false,
        shared.as_ref(),
        &provider,
        channel_id,
        prepared.as_ref(),
        panel,
    )
    .expect("flag-off acknowledgement");
    assert!(
        crate::services::discord::status_panel_orphan_store::load_pending(
            &provider,
            &shared.token_hash,
        )
        .is_empty(),
        "flag OFF must not introduce orphan-store side effects"
    );

    let prepared = prepare_watcher_two_message_panel(
        true,
        shared.as_ref(),
        &provider,
        channel_id,
        None,
        None,
        "panel",
    )
    .expect("prepare pending bind");
    acknowledge_watcher_two_message_panel(
        true,
        shared.as_ref(),
        &provider,
        channel_id,
        prepared.as_ref(),
        panel,
    )
    .expect("persist pending bind");
    assert!(
        crate::services::discord::status_panel_orphan_store::load_pending(
            &provider,
            &shared.token_hash,
        )
        .contains(&(channel_id.get(), panel.get()))
    );

    remove_watcher_two_message_panel_orphan_registration(
        true,
        shared.as_ref(),
        &provider,
        channel_id,
        panel,
    );
    assert!(
        crate::services::discord::status_panel_orphan_store::load_pending(
            &provider,
            &shared.token_hash,
        )
        .is_empty(),
        "successful bind/persist must remove the crash-window orphan record"
    );
}

/// #3293: `InflightTurnState::new` resolves the AgentDesk runtime store; the
/// guard keeps this off the live `~/.adk/release`, falling back to a shared
/// throwaway tempdir (#4514). Point `AGENTDESK_ROOT_DIR` at a per-test
/// throwaway dir under the shared env lock so constructing a test inflight is
/// deterministic; restore on drop.
struct RuntimeRootGuard {
    previous: Option<std::ffi::OsString>,
    _root: tempfile::TempDir,
}

impl Drop for RuntimeRootGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn isolate_agentdesk_runtime_root_for_two_message_tests()
-> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
    (
        lock,
        RuntimeRootGuard {
            previous,
            _root: root,
        },
    )
}
