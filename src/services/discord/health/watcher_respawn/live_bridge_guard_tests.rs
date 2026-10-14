use super::*;
use crate::services::discord::input_runtime::fence::{Gate, test_health};
use crate::services::discord::task_supervisor::n4d_spawn_observation::Scope;
use crate::services::discord::tmux::{InstallFixture, n4d2_entry_support as support};
use poise::serenity_prelude::{MessageId, UserId};

fn run<F: std::future::Future<Output = ()>>(
    name: &str,
    channel: u64,
    check: impl FnOnce(InstallFixture) -> F,
) {
    if !support::isolated(module_path!(), name) {
        return;
    }
    let _absence = blocking_lock_watcher_absence_for_test();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (fixture, _discord) = support::fixture(ProviderKind::Claude, channel).await;
            check(fixture).await;
        });
}

fn assert_no_legacy(observed: &discord::inflight::o_seed_observation::Guard) {
    for event in ["legacy_reclaim_enter", "legacy_rebind_enter"] {
        assert_eq!(observed.snapshot().event_count(event), 0, "{event}");
    }
    support::assert_zero_loaders(observed);
}

fn old_row(fx: &InstallFixture) -> (std::path::PathBuf, Vec<u8>) {
    let mut state = discord::inflight::InflightTurnState::new(
        fx.provider.clone(),
        fx.channel.get(),
        None,
        11,
        9001,
        9002,
        "old format".into(),
        None,
        None,
        None,
        None,
        0,
    );
    state.tmux_session_name = Some(fx.session.clone());
    state.output_path = Some(fx.output.clone());
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.relay_owner_kind = discord::inflight::RelayOwnerKind::SessionBoundRelay;
    let mut value = serde_json::to_value(state).unwrap();
    value.as_object_mut().unwrap().remove("turn_nonce");
    value.as_object_mut().unwrap().remove("finalizer_turn_id");
    let bytes = serde_json::to_vec(&value).unwrap();
    let root = discord::inflight::inflight_runtime_root().unwrap();
    assert_eq!(root, support::root().join("runtime/discord_inflight"));
    let row = discord::inflight::inflight_state_path(&root, &fx.provider, fx.channel.get());
    std::fs::create_dir_all(row.parent().unwrap()).unwrap();
    std::fs::write(&row, &bytes).unwrap();
    let decoded: discord::inflight::InflightTurnState = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.provider_kind().as_ref(), Some(&fx.provider));
    assert_eq!(decoded.channel_id, fx.channel.get());
    assert_eq!(
        decoded.tmux_session_name.as_deref(),
        Some(fx.session.as_str())
    );
    assert_eq!(decoded.finalizer_turn_id, 0);
    assert!(decoded.turn_nonce.is_none());
    (row, bytes)
}

#[test]
fn legacy_retry_keeps_input_fence_and_legacy_call_column() {
    run(
        "legacy_retry_keeps_input_fence_and_legacy_call_column",
        632_520_108,
        |fx| async move {
            let _http = discord::shared_state::test_rest::install(fx.http.clone());
            assert!(Arc::ptr_eq(
                &fx.shared.serenity_http_or_token_fallback().unwrap(),
                &fx.http,
            ));
            let (row, bytes) = old_row(&fx);
            let observed = support::observe(&fx);
            WATCHER_ABSENCE.insert(
                WatcherAbsenceKey::new(&fx.provider, fx.channel),
                WatcherAbsenceState::newly_absent(10),
            );
            let gate = Gate::protect(fx.provider.clone(), fx.channel.get()).unwrap();
            let health = test_health::Clear::new(&gate);
            let closing = gate.close().unwrap();
            assert!(
                !retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    10
                )
                .await
            );
            assert_eq!(std::fs::read(&row).unwrap(), bytes);
            assert_no_legacy(&observed);
            closing.abort_close(false).unwrap();
            assert_eq!(gate.mode(), discord::input_runtime::fence::Mode::LegacyOpen);
            drop(closing);
            drop(health);
            drop(gate);
            let scope = Scope::new(&fx.shared);
            assert!(
                retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    10
                )
                .await
            );
            let after = observed.snapshot();
            assert!(
                after.writable_inflight_load_calls > 0,
                "Legacy pin/load must remain available: {after:?}"
            );
            assert_eq!(after.event_count("legacy_reclaim_enter"), 1);
            assert_eq!(after.event_count("legacy_rebind_enter"), 1);
            assert_ne!(
                std::fs::read(&row).unwrap(),
                bytes,
                "Legacy old-format backfill remains a positive effect"
            );
            assert_eq!(scope.submissions(), 1, "Legacy retry must actually submit");
            let rebound: discord::inflight::InflightTurnState =
                serde_json::from_slice(&std::fs::read(&row).unwrap()).unwrap();
            assert_eq!(rebound.finalizer_turn_id, 9001);
            assert_eq!(
                rebound.effective_relay_owner_kind(),
                discord::inflight::RelayOwnerKind::Watcher,
            );
            assert_eq!(rebound.session_id.as_deref(), Some("e2e"));
            scope.cancel_and_join_all().await;
        },
    );
}

#[test]
fn legacy_public_force_clean_keeps_mailbox_release_and_rebind() {
    run(
        "legacy_public_force_clean_keeps_mailbox_release_and_rebind",
        632_520_111,
        |fx| async move {
            let _http = discord::shared_state::test_rest::install(fx.http.clone());
            assert!(Arc::ptr_eq(
                &fx.shared.serenity_http_or_token_fallback().unwrap(),
                &fx.http,
            ));
            let token = Arc::new(crate::services::provider::CancelToken::new());
            token.bind_unmanaged_session_name(&fx.session);
            assert!(
                discord::mailbox_try_start_turn(
                    &fx.shared,
                    fx.channel,
                    token.clone(),
                    UserId::new(11),
                    MessageId::new(9001)
                )
                .await
            );
            fx.shared.restart.global_active.store(1, Ordering::Release);
            let (row, bytes) = old_row(&fx);
            let mailbox = discord::mailbox_snapshot(&fx.shared, fx.channel).await;
            let mut snapshot = super::super::tests::snapshot(
                fx.channel.get(),
                Some(&fx.session),
                Some(true),
                true,
                Some(9001),
            );
            snapshot.mailbox_active_turn_nonce = mailbox.active_turn_nonce.clone();
            let observed = support::observe(&fx);
            let scope = Scope::new(&fx.shared);
            complete_force_clean_watcher_recovery(
                &fx.registry,
                &fx.provider,
                &fx.shared,
                fx.channel,
                &snapshot,
                100,
                Instant::now(),
            )
            .await;
            assert!(
                token.cancelled.load(Ordering::Acquire),
                "Legacy force-clean still cancels the stale token"
            );
            assert_eq!(fx.shared.restart.global_active.load(Ordering::Acquire), 0);
            assert_eq!(
                discord::mailbox_snapshot(&fx.shared, fx.channel)
                    .await
                    .active_user_message_id,
                None
            );
            let after = observed.snapshot();
            assert_eq!(after.event_count("legacy_rebind_enter"), 1);
            assert!(
                after.writable_inflight_load_calls > 0,
                "Legacy rebind still reads its row: {after:?}"
            );
            assert_ne!(std::fs::read(&row).unwrap(), bytes);
            assert_eq!(
                scope.submissions(),
                1,
                "Legacy force-clean must actually submit"
            );
            let rebound: discord::inflight::InflightTurnState =
                serde_json::from_slice(&std::fs::read(&row).unwrap()).unwrap();
            assert_eq!(rebound.finalizer_turn_id, 9001);
            assert_eq!(
                rebound.effective_relay_owner_kind(),
                discord::inflight::RelayOwnerKind::Watcher,
            );
            assert_eq!(rebound.session_id.as_deref(), Some("e2e"));
            scope.cancel_and_join_all().await;
        },
    );
}
