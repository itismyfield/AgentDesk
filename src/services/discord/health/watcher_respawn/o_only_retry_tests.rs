use super::*;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
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
fn n4d2_retry_before_input_admission_spawns_and_decodes_without_legacy_seed() {
    run(
        "n4d2_retry_before_input_admission_spawns_and_decodes_without_legacy_seed",
        632_520_101,
        |fx| async move {
            let gate = Gate::protect(fx.provider.clone(), fx.channel.get()).unwrap();
            let _health = test_health::Clear::new(&gate);
            let _closing = gate.close().unwrap();
            let (row, bytes) = old_row(&fx);
            let observed = support::observe(&fx);
            let scope = Scope::new(&fx.shared);
            n4d2_arm_absence(&fx.provider, fx.channel, 100);
            let before = n4d2_absence(&fx.provider, fx.channel);
            assert!(
                !retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    100
                )
                .await,
                "before retirement the due retry must retain the closed Legacy fence"
            );
            assert_eq!(n4d2_absence(&fx.provider, fx.channel), before);
            assert_eq!(scope.submissions(), 0);
            assert_no_legacy(&observed);
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let attempted = retry_pending_watcher_respawn(
                &fx.registry,
                &fx.provider,
                std::slice::from_ref(&fx.shared),
                fx.channel,
                100,
            )
            .await;
            assert_no_legacy(&observed);
            assert_eq!(
                scope.submissions(),
                1,
                "the public retry must submit the O watcher"
            );
            assert!(attempted);
            assert!(n4d2_absence(&fx.provider, fx.channel).is_none());
            support::append(&fx, b"{\"type\":\"system\",\"sessionId\":\"e2e\"}\n");
            support::until(&observed, "first source decode", |s| {
                s.decoded_source_chunks > 0
            })
            .await;
            n4d2_arm_absence(&fx.provider, fx.channel, 101);
            assert!(
                retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    101
                )
                .await
            );
            assert_eq!(scope.submissions(), 1, "AlreadyLive must submit nothing");
            assert_no_legacy(&observed);
            assert_eq!(std::fs::read(&row).unwrap(), bytes);
            assert_eq!(scope.cancel_and_join_all().await.len(), 1);
        },
    );
}

#[test]
fn n4d2_force_clean_preserves_mailbox_token_counter_and_row() {
    run(
        "n4d2_force_clean_preserves_mailbox_token_counter_and_row",
        632_520_102,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
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
            let snapshot = super::super::tests::snapshot(
                fx.channel.get(),
                Some(&fx.session),
                Some(true),
                false,
                Some(9001),
            );
            let (row, bytes) = old_row(&fx);
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
            assert_eq!(
                scope.submissions(),
                1,
                "force-clean must install through the O entry"
            );
            assert_eq!(
                discord::mailbox_snapshot(&fx.shared, fx.channel)
                    .await
                    .active_user_message_id,
                Some(MessageId::new(9001))
            );
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert_eq!(fx.shared.restart.global_active.load(Ordering::Acquire), 1);
            assert_no_legacy(&observed);
            assert_eq!(std::fs::read(&row).unwrap(), bytes);
            assert_eq!(scope.cancel_and_join_all().await.len(), 1);
        },
    );
}

#[test]
fn n4d2_failure_keeps_first_seen_reason_budget_due_and_unknown() {
    run(
        "n4d2_failure_keeps_first_seen_reason_budget_due_and_unknown",
        632_520_103,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let generation = tc::session_temp_path(&fx.session, "generation");
            std::fs::remove_file(&generation).unwrap();
            let scope = Scope::new(&fx.shared);
            let observed = support::observe(&fx);
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
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
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel),
                Some((10, 1, 40, Some(OOnlyInstallReason::SourceUnavailable)))
            );
            assert!(
                !retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    39
                )
                .await
            );
            assert_eq!(n4d2_absence(&fx.provider, fx.channel).unwrap().1, 1);
            support::list_sessions(&[]);
            assert!(
                retry_pending_watcher_respawn(&fx.registry, &fx.provider, &[], fx.channel, 40)
                    .await
            );
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel),
                Some((10, 2, 100, Some(OOnlyInstallReason::SourceUnavailable)))
            );
            gc_watcher_absence_state(40 + WATCHER_ABSENCE_STATE_TTL_SECS as i64 + 1);
            assert!(
                n4d2_absence(&fx.provider, fx.channel).is_some(),
                "Unknown never retires an O obligation"
            );
            support::list_sessions(&[&fx.session]);
            std::fs::write(&generation, "restored").unwrap();
            assert!(
                retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    100
                )
                .await
            );
            assert_eq!(scope.submissions(), 1);
            assert!(n4d2_absence(&fx.provider, fx.channel).is_none());
            assert_no_legacy(&observed);
            scope.cancel_and_join_all().await;
        },
    );
}

#[test]
fn n4d2_rowless_observer_uses_durable_binding_and_preserves_deadman() {
    run(
        "n4d2_rowless_observer_uses_durable_binding_and_preserves_deadman",
        632_520_104,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let gate = Gate::protect(fx.provider.clone(), fx.channel.get()).unwrap();
            let _health = test_health::Clear::new(&gate);
            let _closing = gate.close().unwrap();
            let observed = support::observe(&fx);
            assert!(fx.shared.core.lock().await.sessions.is_empty());
            let none = std::collections::HashSet::new();
            assert_eq!(
                observe_routable_unwatched_tui_sessions(
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    &none,
                    10
                )
                .await,
                1
            );
            assert_eq!(n4d2_absence(&fx.provider, fx.channel).unwrap().0, 10);
            let snapshot = super::super::tests::snapshot(
                fx.channel.get(),
                Some(&fx.session),
                Some(true),
                false,
                None,
            );
            assert!(!detect_and_escalate_watcher_absence(
                &fx.provider,
                fx.channel,
                &snapshot,
                false,
                11
            ));
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel).map(|state| state.0),
                Some(10),
                "retired snapshot must retain rowless absence"
            );
            assert_eq!(
                observe_routable_unwatched_tui_sessions(
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    &none,
                    190
                )
                .await,
                1
            );
            assert!(
                WATCHER_ABSENCE
                    .get(&WatcherAbsenceKey::new(&fx.provider, fx.channel))
                    .unwrap()
                    .escalated
            );
            let scope = Scope::new(&fx.shared);
            assert_eq!(
                retry_pending_watcher_respawns(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    190
                )
                .await,
                1
            );
            assert_eq!(scope.submissions(), 1);
            assert_no_legacy(&observed);
            scope.cancel_and_join_all().await;
        },
    );
}

#[test]
fn n4d2_retry_and_force_clean_overlap_submit_once() {
    run(
        "n4d2_retry_and_force_clean_overlap_submit_once",
        632_520_105,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let observed = support::observe(&fx);
            let scope = Scope::new(&fx.shared);
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
            let paused = discord::tmux::n4d2_pause_install_after_host(fx.channel);
            let runtimes = [fx.shared.clone()];
            let retry = retry_pending_watcher_respawn(
                &fx.registry,
                &fx.provider,
                &runtimes,
                fx.channel,
                10,
            );
            tokio::pin!(retry);
            tokio::select! { _ = paused.entered.notified() => {}, result = &mut retry => panic!("missed installer host await: {result}") }
            let snapshot = super::super::tests::snapshot(
                fx.channel.get(),
                Some(&fx.session),
                Some(true),
                false,
                None,
            );
            complete_force_clean_watcher_recovery(
                &fx.registry,
                &fx.provider,
                &fx.shared,
                fx.channel,
                &snapshot,
                10,
                Instant::now(),
            )
            .await;
            assert_eq!(scope.submissions(), 1);
            paused.resume.notify_one();
            assert!(retry.await);
            assert_eq!(scope.submissions(), 1);
            assert!(n4d2_absence(&fx.provider, fx.channel).is_none());
            assert_no_legacy(&observed);
            assert_eq!(scope.cancel_and_join_all().await.len(), 1);
        },
    );
}

#[test]
fn n4d2_retry_foreign_incumbent_is_failure_without_fallback() {
    run(
        "n4d2_retry_foreign_incumbent_is_failure_without_fallback",
        632_520_106,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let observed = support::observe(&fx);
            let scope = Scope::new(&fx.shared);
            assert_eq!(fx.install().await, OOnlyInstallOutcome::Spawned);
            let handle = fx.handle();
            let cancel = handle.cancel.clone();
            let foreign = ChannelId::new(fx.channel.get() + 1_000);
            fx.shared.tmux_watchers.remove(&fx.channel);
            fx.shared.tmux_watchers.insert(foreign, handle);
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
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
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel),
                Some((10, 1, 40, Some(OOnlyInstallReason::SourceMismatch)))
            );
            assert_eq!(
                scope.submissions(),
                1,
                "foreign owner cannot become a second submission"
            );
            assert!(Arc::ptr_eq(
                &fx.shared.tmux_watchers.get(&foreign).unwrap().cancel,
                &cancel
            ));
            assert!(!cancel.load(Ordering::Acquire));
            assert_no_legacy(&observed);
            scope.cancel_and_join_all().await;
        },
    );
}

#[test]
fn n4d2_failure_cap_and_direct_resume_reason_are_not_success() {
    run(
        "n4d2_failure_cap_and_direct_resume_reason_are_not_success",
        632_520_107,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
            record_retired_o_install_outcome(
                &fx.provider,
                fx.channel,
                OOnlyInstallOutcome::Failed(OOnlyInstallReason::RequiresDirectResumeCommit),
                10,
            );
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel),
                Some((
                    10,
                    1,
                    40,
                    Some(OOnlyInstallReason::RequiresDirectResumeCommit)
                ))
            );
            for now in 11..18 {
                record_retired_o_install_outcome(
                    &fx.provider,
                    fx.channel,
                    OOnlyInstallOutcome::Failed(OOnlyInstallReason::RequiresDirectResumeCommit),
                    now,
                );
            }
            let prior = n4d2_absence(&fx.provider, fx.channel).unwrap();
            let scope = Scope::new(&fx.shared);
            assert!(
                !retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    100_000
                )
                .await
            );
            assert_eq!(n4d2_absence(&fx.provider, fx.channel).unwrap(), prior);
            assert_eq!(scope.submissions(), 0);
        },
    );
}

#[test]
fn n4d2_nonretired_retry_keeps_input_fence_and_legacy_call_column() {
    run(
        "n4d2_nonretired_retry_keeps_input_fence_and_legacy_call_column",
        632_520_108,
        |fx| async move {
            let _http = discord::shared_state::test_rest::install(fx.http.clone());
            assert!(Arc::ptr_eq(
                &fx.shared.serenity_http_or_token_fallback().unwrap(),
                &fx.http,
            ));
            let (row, bytes) = old_row(&fx);
            let observed = support::observe(&fx);
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
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
fn n4d2_public_retry_watch_withheld_retains_reason_and_failure_budget() {
    run(
        "n4d2_public_retry_watch_withheld_retains_reason_and_failure_budget",
        632_520_109,
        |fx| async move {
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let host = tc::session_temp_path(&fx.session, "host_kind");
            std::fs::write(&host, "herdr").unwrap();
            let (row, bytes) = old_row(&fx);
            let scope = Scope::new(&fx.shared);
            let observed = support::observe(&fx);
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
            assert!(
                retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    10,
                )
                .await
            );
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel),
                Some((10, 1, 40, Some(OOnlyInstallReason::WatchWithheld),))
            );
            assert_eq!(
                scope.submissions(),
                0,
                "a withheld claim never submits a watcher"
            );
            assert!(!fx.shared.tmux_watchers.contains_key(&fx.channel));
            assert_eq!(std::fs::read(&row).unwrap(), bytes);
            assert_no_legacy(&observed);
            std::fs::write(host, "tmux").unwrap();
            assert!(
                !retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    39,
                )
                .await
            );
            assert_eq!(n4d2_absence(&fx.provider, fx.channel).unwrap().1, 1);
            assert!(
                retry_pending_watcher_respawn(
                    &fx.registry,
                    &fx.provider,
                    std::slice::from_ref(&fx.shared),
                    fx.channel,
                    40,
                )
                .await
            );
            assert_eq!(scope.submissions(), 1);
            assert!(n4d2_absence(&fx.provider, fx.channel).is_none());
            assert_no_legacy(&observed);
            assert_eq!(scope.cancel_and_join_all().await.len(), 1);
        },
    );
}

#[test]
fn n4d2_natural_death_sweep_recovers_after_failure_backoff_and_spawns_once() {
    run(
        "n4d2_natural_death_sweep_recovers_after_failure_backoff_and_spawns_once",
        632_520_110,
        |fx| async move {
            use crate::services::discord::task_supervisor::watcher_completion::Outcome;
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let (row, bytes) = old_row(&fx);
            let observed = support::observe(&fx);
            let scope = Scope::new(&fx.shared);
            assert_eq!(fx.install().await, OOnlyInstallOutcome::Spawned);
            support::until(&observed, "actual idle watcher before natural death", |s| {
                s.event_count("rowless_host_probe") > 0
            })
            .await;
            let dead_cancel = fx.handle().cancel.clone();
            support::pane("dead");
            let dead_marker = tc::session_dead_marker_path(&fx.session);
            std::fs::write(&dead_marker, "dead").unwrap();
            let joined = scope.join_all().await;
            assert_eq!(joined.len(), 1);
            assert_eq!(joined[0].1, Outcome::Returned);
            assert!(
                !dead_cancel.load(Ordering::Acquire),
                "first watcher died without test cancellation"
            );
            assert!(!fx.shared.tmux_watchers.contains_key(&fx.channel));
            support::pane("busy");
            std::fs::remove_file(dead_marker).unwrap();
            let generation = tc::session_temp_path(&fx.session, "generation");
            std::fs::remove_file(&generation).unwrap();
            let runtimes = [fx.shared.clone()];
            let none = std::collections::HashSet::new();
            sweep_and_retry_absences(&fx.registry, &fx.provider, &runtimes, &none, 100).await;
            assert_eq!(
                scope.submissions(),
                1,
                "source admission failure cannot submit recovery"
            );
            assert_eq!(
                n4d2_absence(&fx.provider, fx.channel),
                Some((100, 1, 130, Some(OOnlyInstallReason::SourceUnavailable)))
            );
            sweep_and_retry_absences(&fx.registry, &fx.provider, &runtimes, &none, 101).await;
            assert_eq!(n4d2_absence(&fx.provider, fx.channel).unwrap().1, 1);
            std::fs::write(generation, "recovered").unwrap();
            sweep_and_retry_absences(&fx.registry, &fx.provider, &runtimes, &none, 129).await;
            assert_eq!(scope.submissions(), 1);
            sweep_and_retry_absences(&fx.registry, &fx.provider, &runtimes, &none, 130).await;
            assert_eq!(
                scope.submissions(),
                2,
                "first due tick adds exactly one actual watcher"
            );
            assert!(n4d2_absence(&fx.provider, fx.channel).is_none());
            sweep_and_retry_absences(&fx.registry, &fx.provider, &runtimes, &none, 131).await;
            assert_eq!(
                scope.submissions(),
                2,
                "next tick submits no second recovery"
            );
            support::append(&fx, b"{\"type\":\"system\",\"sessionId\":\"e2e\"}\n");
            support::until(&observed, "actual recovered watcher decode", |s| {
                s.decoded_source_chunks > 0
            })
            .await;
            assert_no_legacy(&observed);
            assert_eq!(std::fs::read(row).unwrap(), bytes);
            let joined = scope.cancel_and_join_all().await;
            assert_eq!(joined.len(), 2);
            assert!(
                joined
                    .iter()
                    .all(|(_, outcome)| *outcome == Outcome::Returned)
            );
        },
    );
}

#[test]
fn n4d2_nonretired_public_force_clean_keeps_mailbox_release_and_rebind() {
    run(
        "n4d2_nonretired_public_force_clean_keeps_mailbox_release_and_rebind",
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

#[test]
fn n4d2_public_retry_pg_unknown_host_uses_actual_installer_and_watcher() {
    const NAME: &str = "n4d2_public_retry_pg_unknown_host_uses_actual_installer_and_watcher";
    if !support::isolated(module_path!(), NAME) {
        return;
    }
    let _absence = blocking_lock_watcher_absence_for_test();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use crate::db::dispatched_sessions::hosted_execution::tests::{future_schema, owner};
            use crate::services::discord::tmux::{WatchHost, watch_host_of};
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let (fx, _discord) =
                support::fixture_with_pool(ProviderKind::Claude, 632_520_112, Some(pool.clone()))
                    .await;
            let key =
                discord::host_teardown_gate::test_support::channel_key(&fx.shared, &fx.session);
            let mut current_owner = owner(&fx.channel.get().to_string());
            current_owner.discord_token_hash = fx.shared.token_hash.clone();
            current_owner.logical_key = fx.session.clone();
            let raw = future_schema(&current_owner);
            discord::inflight::seed_session_row_hashed(
                &pool,
                &key,
                fx.channel.get(),
                &fx.shared.token_hash,
                Some(raw),
            )
            .await;
            assert_eq!(
                watch_host_of(&fx.shared, &fx.provider, fx.channel.get(), &fx.session).await,
                WatchHost::Unverified
            );
            let _retired = RetiredForTest::new("claude", fx.channel.get());
            let observed = support::observe(&fx);
            let scope = Scope::new(&fx.shared);
            n4d2_arm_absence(&fx.provider, fx.channel, 10);
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
            assert_eq!(scope.submissions(), 1);
            assert!(n4d2_absence(&fx.provider, fx.channel).is_none());
            support::until(
                &observed,
                "actual watcher retained its unknown host snapshot",
                |s| s.event_count("outer_eof") >= 2 && s.event_count("host_probe") >= 2,
            )
            .await;
            assert_eq!(observed.snapshot().event_count("rowless_host_probe"), 0);
            support::append(&fx, b"{\"type\":\"system\",\"sessionId\":\"e2e\"}\n");
            support::until(&observed, "unknown host actual source decode", |s| {
                s.decoded_source_chunks > 0
            })
            .await;
            assert_no_legacy(&observed);
            assert_eq!(scope.cancel_and_join_all().await.len(), 1);
            pool.close().await;
            db.drop().await;
        });
}
