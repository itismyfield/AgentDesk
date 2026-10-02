//! Orphan-token recovery on every stored host case and tmux answer, through its real entries.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::auto_heal_attempts::{
    auto_heal_attempt_counters_for_tests, refund_auto_heal_attempt,
};
use super::super::{
    AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW, HealthRegistry,
    ORPHAN_PENDING_TOKEN_ADMISSION_GRACE, RelayRecoveryActionKind, RelayRecoveryApplySource,
    SharedData, apply_relay_recovery_plan, auto_heal_key, auto_heal_test_lock,
    clear_auto_heal_attempts_for_tests, reserve_auto_heal_attempt, run_relay_recovery_at,
};
use crate::services::discord::admin_host_guard::tests::{Host, ModeTmux, Tmux};
use crate::services::discord::host_defer_gate::tests::{Case, map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, shared_on};
use crate::services::provider::cancel_token_cleanup::authority::TmuxBinding;
use crate::services::provider::{CancelToken, ProviderKind};

/// A busy turn on `channel` whose token is bound to `name`, with the counter it holds.
async fn orphan_turn(shared: &Arc<SharedData>, channel: ChannelId, name: &str) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let binding = TmuxBinding::NameOnly {
        name: name.to_string(),
    };
    *token.tmux_binding.lock().unwrap() = Some(binding);
    let message = MessageId::new(channel.get() + 1);
    let started = crate::services::discord::mailbox_try_start_turn;
    assert!(started(shared, channel, token.clone(), UserId::new(7), message).await);
    shared.restart.global_active.store(1, Ordering::Relaxed);
    token
}

/// Whether the turn still owns its mailbox, uncancelled, with the counter it holds.
async fn turn_kept(shared: &Arc<SharedData>, channel: ChannelId, token: &Arc<CancelToken>) -> bool {
    let after = crate::services::discord::mailbox_snapshot(shared, channel).await;
    let owned = after
        .cancel_token
        .as_ref()
        .is_some_and(|t| Arc::ptr_eq(t, token));
    owned
        && after.active_user_message_id == Some(MessageId::new(channel.get() + 1))
        && !token.cancelled.load(Ordering::Relaxed)
        && shared.restart.global_active.load(Ordering::Relaxed) == 1
}

/// A planning instant past the admission grace of every turn started so far.
fn past_admission_grace() -> i64 {
    let grace = ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64;
    chrono::Utc::now().timestamp_millis() + grace
}

/// The `(attempts, consecutive_refunds)` a recovery lane holds for `channel`.
fn budget(channel: ChannelId, source: RelayRecoveryApplySource) -> (u32, u32) {
    let action = RelayRecoveryActionKind::ClearOrphanPendingToken;
    let key = auto_heal_key("claude", channel.get(), action, source);
    let counters = auto_heal_attempt_counters_for_tests(&key).expect("a budget window");
    (counters.attempts, counters.consecutive_refunds)
}

/// One failed earlier attempt on the lane, so a commit or a refund would move its streak.
fn earlier_refund(channel: ChannelId, source: RelayRecoveryApplySource, now_ms: i64) {
    let action = RelayRecoveryActionKind::ClearOrphanPendingToken;
    let key = auto_heal_key("claude", channel.get(), action, source);
    reserve_auto_heal_attempt(&key, now_ms, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW).unwrap();
    refund_auto_heal_attempt(&key, now_ms);
    assert_eq!(budget(channel, source), (0, 1));
}

// Manual recovery of an orphan token releases the mailbox, the token and the counter only for
// a confirmed legacy session; any other host keeps all three and gives its attempt back.
#[tokio::test]
async fn manual_orphan_recovery_keeps_a_turn_on_an_unconfirmed_host_pg() {
    let _lock = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let registry = HealthRegistry::new();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    let provider = ProviderKind::Claude;
    let source = RelayRecoveryApplySource::Manual;
    for (m, mode) in Tmux::ALL.into_iter().enumerate() {
        tmux.serve(mode);
        for (n, host) in Host::ALL.into_iter().enumerate() {
            let label = format!("{mode:?} {host:?}");
            let channel = ChannelId::new(1_479_671_340_000_000_000 + (m * 100 + n) as u64);
            let channel_name = format!("p4r1b-orphan-{m}-{n}");
            let name = provider.build_tmux_session_name(&channel_name);
            map_channel(&shared, channel, &channel_name).await;
            host.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
                .await;
            let token = orphan_turn(&shared, channel, &name).await;
            let now_ms = past_admission_grace();
            earlier_refund(channel, source, now_ms);
            tmux.take_writes();

            let response =
                run_relay_recovery_at(&registry, Some("claude"), channel.get(), true, now_ms)
                    .await
                    .expect("manual recovery should evaluate");

            let measured_dead = host.probed() && matches!(mode, Tmux::Dead | Tmux::NoSocket);
            if host.admitted() && measured_dead {
                assert!(
                    response.applied,
                    "{label}: {:?}",
                    response.decision.auto_heal
                );
                assert!(token.cancelled.load(Ordering::Relaxed), "{label}");
                let counter = shared.restart.global_active.load(Ordering::Relaxed);
                assert_eq!(counter, 0, "{label}");
                continue;
            }
            assert!(!response.applied, "{label}");
            assert!(turn_kept(&shared, channel, &token).await, "{label}");
            assert_eq!(tmux.take_writes(), Vec::<String>::new(), "{label}");
            if !measured_dead {
                continue;
            }
            let result = response.apply_result.as_ref().expect("an apply result");
            assert_eq!(result.status, "host_deferred", "{label}");
            assert!(!result.removed_mailbox_token, "{label}");
            assert!(response.skipped && !response.ok, "{label}");
            let reason = response.decision.auto_heal.skipped_reason;
            assert_eq!(reason, Some("host_not_legacy_tmux"), "{label}");
            assert_eq!(
                budget(channel, source),
                (0, 1),
                "{label}: attempt back, streak kept"
            );
        }
    }
    db.drop().await;
}

// The probe and watchdog lanes reach the same apply with their own budget: a session on an
// unconfirmed host keeps its turn, and the lane's attempt goes back with its streak as it was.
#[tokio::test]
async fn automatic_orphan_apply_keeps_a_turn_on_an_unconfirmed_host_pg() {
    let _lock = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    tmux.serve(Tmux::Dead);
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let registry = HealthRegistry::new();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    let provider = ProviderKind::Claude;
    let sources = [
        RelayRecoveryApplySource::StallWatchdog,
        RelayRecoveryApplySource::ProbeAutoHeal,
    ];
    let hosts = [Stored::Legacy, Stored::Hosted, Stored::Future];
    for (s, source) in sources.into_iter().enumerate() {
        for (n, stored) in hosts.into_iter().enumerate() {
            let label = format!("{source:?} {stored:?}");
            let channel = ChannelId::new(1_479_671_341_000_000_000 + (s * 100 + n) as u64);
            let channel_name = format!("p4r1b-auto-{s}-{n}");
            let name = provider.build_tmux_session_name(&channel_name);
            map_channel(&shared, channel, &channel_name).await;
            let host = Host::Case(Case::Stored(stored));
            host.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
                .await;
            let token = orphan_turn(&shared, channel, &name).await;
            let now_ms = past_admission_grace();
            let planned =
                run_relay_recovery_at(&registry, Some("claude"), channel.get(), false, now_ms);
            let decision = planned.await.expect("a plan").decision;
            assert!(
                decision.auto_heal.eligible,
                "{label}: a measured dead orphan"
            );
            earlier_refund(channel, source, now_ms);

            let response =
                apply_relay_recovery_plan(&registry, &shared, &provider, decision, now_ms, source)
                    .await;

            if host.admitted() {
                assert!(response.applied, "{label}");
                assert!(token.cancelled.load(Ordering::Relaxed), "{label}");
                continue;
            }
            let result = response.apply_result.as_ref().expect("an apply result");
            assert_eq!(result.status, "host_deferred", "{label}");
            assert!(!response.applied && response.skipped, "{label}");
            assert!(turn_kept(&shared, channel, &token).await, "{label}");
            assert_eq!(
                budget(channel, source),
                (0, 1),
                "{label}: attempt back, streak kept"
            );
        }
    }
    db.drop().await;
}
